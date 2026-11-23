//! Everything the bot remembers, behind one trait. Postgres in production,
//! an in-memory fake in tests; both pass the same test suite.

pub mod memory;
pub mod pg;

use async_trait::async_trait;

use crate::model::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

pub type StoreResult<T> = Result<T, StoreError>;

/// Row counts for `/health`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub entities: i64,
    pub wallets: i64,
    pub trades: i64,
    pub transfers: i64,
}

#[async_trait]
pub trait Store: Send + Sync {
    // ---- accounts and access ----

    /// Creates the account if missing, and keeps its name and admin flag current.
    async fn upsert_account(&self, id: AccountId, name: &str, is_admin: bool, now: i64) -> StoreResult<Account>;
    async fn account(&self, id: AccountId) -> StoreResult<Option<Account>>;
    async fn accounts(&self) -> StoreResult<Vec<Account>>;
    async fn set_paused(&self, id: AccountId, paused: bool) -> StoreResult<()>;
    async fn create_invite(&self, code: &str, uses: i32, created_by: AccountId, now: i64) -> StoreResult<()>;
    /// Spends one use of the code. False when it does not exist or is used up.
    async fn redeem_invite(&self, code: &str) -> StoreResult<bool>;

    // ---- entities ----

    async fn create_entity(&self, label: &str, created_by: AccountId, now: i64) -> StoreResult<Entity>;
    async fn entity(&self, id: EntityId) -> StoreResult<Option<Entity>>;
    /// Every entity, oldest first.
    async fn entities(&self) -> StoreResult<Vec<Entity>>;
    async fn rename_entity(&self, id: EntityId, label: &str) -> StoreResult<bool>;
    async fn set_entity_status(&self, id: EntityId, status: EntityStatus) -> StoreResult<bool>;

    // ---- wallets ----

    /// Inserts the wallet, or updates its entity, state, score and link
    /// reason. `first_seen` and the backfill cursor of an existing row are kept.
    async fn put_wallet(&self, wallet: &Wallet) -> StoreResult<()>;
    async fn wallet(&self, address: &str) -> StoreResult<Option<Wallet>>;
    async fn wallets_of(&self, entity: EntityId) -> StoreResult<Vec<Wallet>>;
    /// Linked and candidate wallets of entities that are still tracked:
    /// exactly the set the stream subscribes to.
    async fn watched_wallets(&self) -> StoreResult<Vec<Wallet>>;
    async fn set_cursor(&self, address: &str, signature: &str) -> StoreResult<()>;

    // ---- evidence ----

    async fn add_evidence(&self, evidence: &NewEvidence) -> StoreResult<i64>;
    /// Newest first, voided rows included.
    async fn evidence_of_wallet(&self, address: &str) -> StoreResult<Vec<Evidence>>;
    /// Voids every live row tying this wallet to this entity. Returns how many.
    async fn void_evidence(&self, address: &str, entity: EntityId) -> StoreResult<u64>;

    // ---- follows ----

    async fn set_follow(&self, account: AccountId, entity: EntityId, mode: FollowMode) -> StoreResult<()>;
    async fn follows_of_entity(&self, entity: EntityId) -> StoreResult<Vec<Follow>>;
    async fn follows_of_account(&self, account: AccountId) -> StoreResult<Vec<Follow>>;

    // ---- labels ----

    async fn put_label(&self, label: &Label) -> StoreResult<()>;
    async fn delete_label(&self, address: &str) -> StoreResult<bool>;
    async fn labels(&self) -> StoreResult<Vec<Label>>;

    // ---- what the watcher saw ----

    /// Stores the trade once. False when this (signature, wallet, mint) is
    /// already there: that answer is the claim-once rule for alerts.
    async fn insert_trade(&self, trade: &Trade) -> StoreResult<bool>;
    /// Trades by the wallets linked to the entity right now, newest first.
    async fn trades_of_entity(&self, entity: EntityId, limit: i64) -> StoreResult<Vec<Trade>>;
    async fn trades_of_wallet(&self, address: &str, limit: i64) -> StoreResult<Vec<Trade>>;
    async fn insert_transfer(&self, transfer: &Transfer) -> StoreResult<bool>;
    async fn transfers_of_wallet(&self, address: &str, limit: i64) -> StoreResult<Vec<Transfer>>;

    // ---- signals ----

    /// The newest signal for this entity, mint and side whose last trade is
    /// at or after `since`.
    async fn open_signal(&self, entity: EntityId, mint: &str, side: Side, since: i64) -> StoreResult<Option<Signal>>;
    /// Stores a new signal; the `id` passed in is ignored.
    async fn create_signal(&self, signal: &Signal) -> StoreResult<Signal>;
    async fn update_signal(&self, signal: &Signal) -> StoreResult<()>;
    async fn set_signal_message(&self, signal: i64, account: AccountId, message_id: i64) -> StoreResult<()>;
    async fn signal_messages(&self, signal: i64) -> StoreResult<Vec<(AccountId, i64)>>;

    // ---- small state ----

    async fn get_state(&self, key: &str) -> StoreResult<Option<String>>;
    async fn set_state(&self, key: &str, value: &str) -> StoreResult<()>;
    async fn add_credits(&self, day: &str, lane: &str, credits: i64) -> StoreResult<()>;
    async fn credits(&self, day: &str) -> StoreResult<Vec<(String, i64)>>;
    async fn counts(&self) -> StoreResult<Counts>;

    // ---- identity, continued ----

    async fn void_evidence_row(&self, id: i64) -> StoreResult<bool>;
    /// Sets the points of every live row of a kind. Returns how many changed.
    async fn repoint_evidence(&self, kind: &str, points: i32) -> StoreResult<u64>;
    /// Every row tying any wallet to this entity, newest first, voided included.
    async fn evidence_of_entity(&self, entity: EntityId) -> StoreResult<Vec<Evidence>>;
    /// Re-points every evidence row from one entity to another (a merge).
    async fn move_evidence(&self, from: EntityId, into: EntityId) -> StoreResult<u64>;
    async fn wallets_in_state(&self, state: WalletState) -> StoreResult<Vec<Wallet>>;
    /// `None` leaves a flag as it is.
    async fn set_entity_flags(&self, id: EntityId, copyable: Option<bool>, bait: Option<bool>) -> StoreResult<()>;
    /// Trades of one token seen at or after `since`, oldest first.
    async fn trades_of_mint(&self, mint: &str, since: i64, limit: i64) -> StoreResult<Vec<Trade>>;
    /// Signals whose last trade is at or after `since`, oldest first.
    async fn signals_since(&self, since: i64) -> StoreResult<Vec<Signal>>;
    async fn signal(&self, id: i64) -> StoreResult<Option<Signal>>;

    // ---- deposit addresses ----

    async fn put_deposit(&self, deposit: &DepositAddr) -> StoreResult<()>;
    async fn deposit(&self, address: &str) -> StoreResult<Option<DepositAddr>>;
    async fn deposits_of(&self, entity: EntityId) -> StoreResult<Vec<DepositAddr>>;
    /// Qualified deposit addresses of entities still tracked: the tripwires.
    async fn watched_deposits(&self) -> StoreResult<Vec<DepositAddr>>;

    // ---- forensics queue ----

    /// Queues a job. `None` when one of the same kind and subject is already waiting or running.
    async fn enqueue_job(&self, job: &NewJob, now: i64) -> StoreResult<Option<i64>>;
    /// Takes the oldest job that is due and marks it running.
    async fn claim_job(&self, now: i64) -> StoreResult<Option<Job>>;
    async fn finish_job(&self, id: i64, ok: bool) -> StoreResult<()>;
    /// Puts a running job back in the queue for later.
    async fn retry_job(&self, id: i64, run_after: i64, payload: &serde_json::Value) -> StoreResult<()>;
    /// `(queued, running)`.
    async fn job_counts(&self) -> StoreResult<(i64, i64)>;

    // ---- documents: small keyed JSON records (caches, settings, reviews) ----

    async fn put_doc(&self, kind: &str, key: &str, value: &serde_json::Value, now: i64) -> StoreResult<()>;
    async fn doc(&self, kind: &str, key: &str) -> StoreResult<Option<serde_json::Value>>;
    /// Documents of a kind whose key starts with `prefix`, by key.
    async fn docs(&self, kind: &str, prefix: &str, limit: i64) -> StoreResult<Vec<(String, serde_json::Value)>>;
    async fn delete_doc(&self, kind: &str, key: &str) -> StoreResult<bool>;

    // ---- positions ----

    /// Stores a new position. `None` when this account already has one for
    /// the signal in this mode: a second copy of one signal is never opened.
    async fn insert_position(&self, position: &Position) -> StoreResult<Option<i64>>;
    async fn update_position(&self, position: &Position) -> StoreResult<()>;
    async fn position(&self, id: i64) -> StoreResult<Option<Position>>;
    /// Every position that is opening, open or unconfirmed.
    async fn open_positions(&self) -> StoreResult<Vec<Position>>;
    /// An account's positions opened at or after `since`, newest first.
    async fn positions_of_account(&self, account: AccountId, since: i64) -> StoreResult<Vec<Position>>;
    async fn positions_of_entity(&self, entity: EntityId, mode: PositionMode) -> StoreResult<Vec<Position>>;

    // ---- leases ----

    /// Takes or renews a named lease. False while someone else holds it.
    async fn acquire_lease(&self, name: &str, holder: &str, ttl_s: i64, now: i64) -> StoreResult<bool>;
}
