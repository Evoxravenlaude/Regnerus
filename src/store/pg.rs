//! The Postgres store (Supabase in production).
//!
//! Every table lives in the bot's own schema, set as the connection's
//! `search_path`, so nothing lands in the schemas Supabase exposes through
//! its Data API. Migrations are applied at start-up under an advisory lock.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::{AssertSqlSafe, Row};

use super::{Counts, Store, StoreError, StoreResult};
use crate::model::*;

const MIGRATIONS: &[(i32, &str, &str)] = &[(1, "init", include_str!("../../migrations/001_init.sql")), (2, "trail_trader", include_str!("../../migrations/002_trail_trader.sql"))];

/// An arbitrary constant: the advisory lock that serialises migrations.
const MIGRATION_LOCK: i64 = 0x79_656b_735f_6d65;

pub struct PgStore {
    pool: PgPool,
}

fn db<E: std::fmt::Display>(error: E) -> StoreError {
    StoreError(error.to_string())
}

fn bad(column: &str, value: &str) -> StoreError {
    StoreError(format!("unexpected value in {column}: {value}"))
}

/// A schema name goes into SQL text, so it is held to a strict shape.
pub fn valid_schema(name: &str) -> bool {
    !name.is_empty() && name.len() <= 48 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit())
}

fn u64_of(column: &str, text: String) -> StoreResult<u64> {
    text.parse().map_err(|_| bad(column, &text))
}

fn strings_of(value: Value) -> Vec<String> {
    value.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// Whether the URL names a pooler that shares server connections between
/// transactions (PgBouncer in transaction mode, Supabase on port 6543).
pub fn is_transaction_pooler(url: &str) -> bool {
    url.contains(":6543") || url.contains("pgbouncer=true")
}

impl PgStore {
    /// Connects, makes sure the schema exists, and points every connection at it.
    pub async fn connect(url: &str, schema: &str) -> StoreResult<Self> {
        if !valid_schema(schema) {
            return Err(StoreError(format!("DB_SCHEMA must be lowercase letters, digits and underscores, got {schema:?}")));
        }
        // A transaction-mode pooler (Supabase's port 6543) hands each
        // transaction to whichever server connection is free. The schema is
        // chosen with a session-level `set search_path`, which such a pooler
        // does not carry from one transaction to the next: queries would
        // land in `public`, or fail, at random. Refuse it up front.
        if is_transaction_pooler(url) {
            return Err(StoreError(
                "DATABASE_URL points at a transaction-mode pooler (port 6543 or pgbouncer=true). Use the direct or session-pooler URL instead (port 5432 on Supabase): the bot sets its schema once per connection.".to_string(),
            ));
        }
        let setup = format!("create schema if not exists \"{schema}\"; set search_path to \"{schema}\"");
        let options: sqlx::postgres::PgConnectOptions = url.parse().map_err(db)?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(15))
            .after_connect(move |conn, _meta| {
                let setup = setup.clone();
                Box::pin(async move {
                    sqlx::raw_sql(AssertSqlSafe(setup)).execute(conn).await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .map_err(db)?;
        Ok(PgStore { pool })
    }

    /// Applies every migration not yet recorded. Returns the versions applied.
    pub async fn migrate(&self) -> StoreResult<Vec<i32>> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("select pg_advisory_xact_lock($1)").bind(MIGRATION_LOCK).execute(&mut *tx).await.map_err(db)?;
        sqlx::raw_sql("create table if not exists schema_migrations (version integer primary key, name text not null, applied_at bigint not null)").execute(&mut *tx).await.map_err(db)?;
        let done: Vec<i32> = sqlx::query("select version from schema_migrations").fetch_all(&mut *tx).await.map_err(db)?.iter().map(|r| r.get(0)).collect();
        let mut applied = Vec::new();
        for (version, name, sql) in MIGRATIONS {
            if done.contains(version) {
                continue;
            }
            sqlx::raw_sql(*sql).execute(&mut *tx).await.map_err(|e| StoreError(format!("migration {version} ({name}): {e}")))?;
            sqlx::query("insert into schema_migrations (version, name, applied_at) values ($1, $2, extract(epoch from now())::bigint)").bind(version).bind(name).execute(&mut *tx).await.map_err(db)?;
            applied.push(*version);
        }
        tx.commit().await.map_err(db)?;
        Ok(applied)
    }

    /// Drops the bot's schema. Tests only.
    pub async fn drop_schema(&self, schema: &str) -> StoreResult<()> {
        if !valid_schema(schema) {
            return Err(StoreError("invalid schema".into()));
        }
        sqlx::raw_sql(AssertSqlSafe(format!("drop schema if exists \"{schema}\" cascade"))).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

fn account_of(row: &PgRow) -> StoreResult<Account> {
    Ok(Account {
        id: row.try_get("id").map_err(db)?,
        name: row.try_get("name").map_err(db)?,
        is_admin: row.try_get("is_admin").map_err(db)?,
        paused: row.try_get("paused").map_err(db)?,
        created_at: row.try_get("created_at").map_err(db)?,
    })
}

fn entity_of(row: &PgRow) -> StoreResult<Entity> {
    let status: String = row.try_get("status").map_err(db)?;
    Ok(Entity {
        id: row.try_get("id").map_err(db)?,
        label: row.try_get("label").map_err(db)?,
        status: EntityStatus::parse(&status).ok_or_else(|| bad("entities.status", &status))?,
        copyable: row.try_get("copyable").map_err(db)?,
        bait: row.try_get("bait").map_err(db)?,
        created_by: row.try_get("created_by").map_err(db)?,
        created_at: row.try_get("created_at").map_err(db)?,
    })
}

fn wallet_of(row: &PgRow) -> StoreResult<Wallet> {
    let state: String = row.try_get("state").map_err(db)?;
    let linked_by: Option<String> = row.try_get("linked_by").map_err(db)?;
    Ok(Wallet {
        address: row.try_get("address").map_err(db)?,
        entity_id: row.try_get("entity_id").map_err(db)?,
        state: WalletState::parse(&state).ok_or_else(|| bad("wallets.state", &state))?,
        score: row.try_get("score").map_err(db)?,
        linked_by: match linked_by {
            Some(text) => Some(LinkedBy::parse(&text).ok_or_else(|| bad("wallets.linked_by", &text))?),
            None => None,
        },
        first_seen: row.try_get("first_seen").map_err(db)?,
        last_signature: row.try_get("last_signature").map_err(db)?,
    })
}

fn evidence_of(row: &PgRow) -> StoreResult<Evidence> {
    Ok(Evidence {
        id: row.try_get("id").map_err(db)?,
        wallet: row.try_get("wallet").map_err(db)?,
        entity_id: row.try_get("entity_id").map_err(db)?,
        kind: row.try_get("kind").map_err(db)?,
        family: row.try_get("family").map_err(db)?,
        points: row.try_get("points").map_err(db)?,
        signatures: strings_of(row.try_get::<Value, _>("signatures").map_err(db)?),
        details: row.try_get("details").map_err(db)?,
        source: row.try_get("source").map_err(db)?,
        voided: row.try_get("voided").map_err(db)?,
        observed_at: row.try_get("observed_at").map_err(db)?,
    })
}

fn label_of(row: &PgRow) -> StoreResult<Label> {
    let class: String = row.try_get("class").map_err(db)?;
    Ok(Label {
        address: row.try_get("address").map_err(db)?,
        class: LabelClass::parse(&class).ok_or_else(|| bad("labels.class", &class))?,
        name: row.try_get("name").map_err(db)?,
        source: row.try_get("source").map_err(db)?,
    })
}

fn trade_of(row: &PgRow) -> StoreResult<Trade> {
    let side: String = row.try_get("side").map_err(db)?;
    let quote: String = row.try_get("quote").map_err(db)?;
    let venue: String = row.try_get("venue").map_err(db)?;
    let token_program: Option<String> = row.try_get("token_program").map_err(db)?;
    let fingerprint: Value = row.try_get("fingerprint").map_err(db)?;
    Ok(Trade {
        signature: row.try_get("signature").map_err(db)?,
        slot: row.try_get::<i64, _>("slot").map_err(db)? as u64,
        block_time: row.try_get("block_time").map_err(db)?,
        seen_at: row.try_get("seen_at").map_err(db)?,
        wallet: row.try_get("wallet").map_err(db)?,
        entity_id: row.try_get("entity_id").map_err(db)?,
        side: Side::parse(&side).ok_or_else(|| bad("trades.side", &side))?,
        mint: row.try_get("mint").map_err(db)?,
        token_amount_raw: u64_of("trades.token_amount_raw", row.try_get("token_amount_raw").map_err(db)?)?,
        decimals: row.try_get::<i32, _>("decimals").map_err(db)? as u8,
        token_program: token_program.as_deref().and_then(TokenProgram::parse),
        quote: Quote::parse(&quote).ok_or_else(|| bad("trades.quote", &quote))?,
        quote_amount_raw: row.try_get::<i64, _>("quote_amount_raw").map_err(db)? as u64,
        programs: strings_of(row.try_get::<Value, _>("programs").map_err(db)?),
        venue: Venue::parse(&venue).ok_or_else(|| bad("trades.venue", &venue))?,
        fingerprint: serde_json::from_value(fingerprint).unwrap_or_default(),
    })
}

fn transfer_of(row: &PgRow) -> StoreResult<Transfer> {
    let direction: String = row.try_get("direction").map_err(db)?;
    let mint: String = row.try_get("mint").map_err(db)?;
    Ok(Transfer {
        signature: row.try_get("signature").map_err(db)?,
        slot: row.try_get::<i64, _>("slot").map_err(db)? as u64,
        block_time: row.try_get("block_time").map_err(db)?,
        wallet: row.try_get("wallet").map_err(db)?,
        direction: Direction::parse(&direction).ok_or_else(|| bad("transfers.direction", &direction))?,
        counterparty: row.try_get("counterparty").map_err(db)?,
        mint: if mint.is_empty() { None } else { Some(mint) },
        amount_raw: u64_of("transfers.amount_raw", row.try_get("amount_raw").map_err(db)?)?,
        decimals: row.try_get::<i32, _>("decimals").map_err(db)? as u8,
    })
}

fn signal_of(row: &PgRow) -> StoreResult<Signal> {
    let side: String = row.try_get("side").map_err(db)?;
    let quote: String = row.try_get("quote").map_err(db)?;
    let venue: String = row.try_get("venue").map_err(db)?;
    Ok(Signal {
        id: row.try_get("id").map_err(db)?,
        entity_id: row.try_get("entity_id").map_err(db)?,
        mint: row.try_get("mint").map_err(db)?,
        side: Side::parse(&side).ok_or_else(|| bad("signals.side", &side))?,
        first_at: row.try_get("first_at").map_err(db)?,
        last_at: row.try_get("last_at").map_err(db)?,
        quote: Quote::parse(&quote).ok_or_else(|| bad("signals.quote", &quote))?,
        quote_amount_raw: row.try_get::<i64, _>("quote_amount_raw").map_err(db)? as u64,
        token_amount_raw: u64_of("signals.token_amount_raw", row.try_get("token_amount_raw").map_err(db)?)?,
        decimals: row.try_get::<i32, _>("decimals").map_err(db)? as u8,
        wallets: strings_of(row.try_get::<Value, _>("wallets").map_err(db)?),
        trades: row.try_get::<i32, _>("trades").map_err(db)? as u32,
        venue: Venue::parse(&venue).ok_or_else(|| bad("signals.venue", &venue))?,
    })
}

/// Lamport and dollar amounts fit a signed 64-bit column with room to spare.
fn i64_of(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[async_trait]
impl Store for PgStore {
    async fn upsert_account(&self, id: AccountId, name: &str, is_admin: bool, now: i64) -> StoreResult<Account> {
        let row = sqlx::query(
            "insert into accounts (id, name, is_admin, paused, created_at) values ($1, $2, $3, false, $4)
             on conflict (id) do update set name = excluded.name, is_admin = excluded.is_admin
             returning id, name, is_admin, paused, created_at",
        )
        .bind(id)
        .bind(name)
        .bind(is_admin)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        account_of(&row)
    }

    async fn account(&self, id: AccountId) -> StoreResult<Option<Account>> {
        let row = sqlx::query("select id, name, is_admin, paused, created_at from accounts where id = $1").bind(id).fetch_optional(&self.pool).await.map_err(db)?;
        row.as_ref().map(account_of).transpose()
    }

    async fn accounts(&self) -> StoreResult<Vec<Account>> {
        let rows = sqlx::query("select id, name, is_admin, paused, created_at from accounts order by created_at, id").fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(account_of).collect()
    }

    async fn set_paused(&self, id: AccountId, paused: bool) -> StoreResult<()> {
        sqlx::query("update accounts set paused = $2 where id = $1").bind(id).bind(paused).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    async fn create_invite(&self, code: &str, uses: i32, created_by: AccountId, now: i64) -> StoreResult<()> {
        sqlx::query("insert into invite_codes (code, uses_left, created_by, created_at) values ($1, $2, $3, $4) on conflict (code) do update set uses_left = excluded.uses_left")
            .bind(code)
            .bind(uses)
            .bind(created_by)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn redeem_invite(&self, code: &str) -> StoreResult<bool> {
        let done = sqlx::query("update invite_codes set uses_left = uses_left - 1 where code = $1 and uses_left > 0").bind(code).execute(&self.pool).await.map_err(db)?;
        Ok(done.rows_affected() == 1)
    }

    async fn create_entity(&self, label: &str, created_by: AccountId, now: i64) -> StoreResult<Entity> {
        let row = sqlx::query("insert into entities (label, created_by, created_at) values ($1, $2, $3) returning id, label, status, copyable, bait, created_by, created_at")
            .bind(label)
            .bind(created_by)
            .bind(now)
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        entity_of(&row)
    }

    async fn entity(&self, id: EntityId) -> StoreResult<Option<Entity>> {
        let row = sqlx::query("select id, label, status, copyable, bait, created_by, created_at from entities where id = $1").bind(id).fetch_optional(&self.pool).await.map_err(db)?;
        row.as_ref().map(entity_of).transpose()
    }

    async fn entities(&self) -> StoreResult<Vec<Entity>> {
        let rows = sqlx::query("select id, label, status, copyable, bait, created_by, created_at from entities order by id").fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(entity_of).collect()
    }

    async fn rename_entity(&self, id: EntityId, label: &str) -> StoreResult<bool> {
        let done = sqlx::query("update entities set label = $2 where id = $1").bind(id).bind(label).execute(&self.pool).await.map_err(db)?;
        Ok(done.rows_affected() == 1)
    }

    async fn set_entity_status(&self, id: EntityId, status: EntityStatus) -> StoreResult<bool> {
        let done = sqlx::query("update entities set status = $2 where id = $1").bind(id).bind(status.as_str()).execute(&self.pool).await.map_err(db)?;
        Ok(done.rows_affected() == 1)
    }

    async fn put_wallet(&self, wallet: &Wallet) -> StoreResult<()> {
        sqlx::query(
            "insert into wallets (address, entity_id, state, score, linked_by, first_seen) values ($1, $2, $3, $4, $5, $6)
             on conflict (address) do update set entity_id = excluded.entity_id, state = excluded.state, score = excluded.score, linked_by = excluded.linked_by",
        )
        .bind(&wallet.address)
        .bind(wallet.entity_id)
        .bind(wallet.state.as_str())
        .bind(wallet.score)
        .bind(wallet.linked_by.map(|l| l.as_str()))
        .bind(wallet.first_seen)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn wallet(&self, address: &str) -> StoreResult<Option<Wallet>> {
        let row =
            sqlx::query("select address, entity_id, state, score, linked_by, first_seen, last_signature from wallets where address = $1").bind(address).fetch_optional(&self.pool).await.map_err(db)?;
        row.as_ref().map(wallet_of).transpose()
    }

    async fn wallets_of(&self, entity: EntityId) -> StoreResult<Vec<Wallet>> {
        let rows = sqlx::query("select address, entity_id, state, score, linked_by, first_seen, last_signature from wallets where entity_id = $1 order by first_seen, address")
            .bind(entity)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.iter().map(wallet_of).collect()
    }

    async fn watched_wallets(&self) -> StoreResult<Vec<Wallet>> {
        let rows = sqlx::query(
            "select w.address, w.entity_id, w.state, w.score, w.linked_by, w.first_seen, w.last_signature
             from wallets w join entities e on e.id = w.entity_id
             where w.state in ('linked', 'candidate') and e.status = 'tracking' order by w.address",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter().map(wallet_of).collect()
    }

    async fn set_cursor(&self, address: &str, signature: &str) -> StoreResult<()> {
        sqlx::query("update wallets set last_signature = $2 where address = $1").bind(address).bind(signature).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    async fn add_evidence(&self, evidence: &NewEvidence) -> StoreResult<i64> {
        let row = sqlx::query(
            "insert into evidence (wallet, entity_id, kind, family, points, signatures, details, source, observed_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) returning id",
        )
        .bind(&evidence.wallet)
        .bind(evidence.entity_id)
        .bind(&evidence.kind)
        .bind(&evidence.family)
        .bind(evidence.points)
        .bind(serde_json::json!(evidence.signatures))
        .bind(&evidence.details)
        .bind(&evidence.source)
        .bind(evidence.observed_at)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        row.try_get("id").map_err(db)
    }

    async fn evidence_of_wallet(&self, address: &str) -> StoreResult<Vec<Evidence>> {
        let rows = sqlx::query("select id, wallet, entity_id, kind, family, points, signatures, details, source, voided, observed_at from evidence where wallet = $1 order by id desc")
            .bind(address)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.iter().map(evidence_of).collect()
    }

    async fn void_evidence(&self, address: &str, entity: EntityId) -> StoreResult<u64> {
        let done = sqlx::query("update evidence set voided = true where wallet = $1 and entity_id = $2 and not voided").bind(address).bind(entity).execute(&self.pool).await.map_err(db)?;
        Ok(done.rows_affected())
    }

    async fn set_follow(&self, account: AccountId, entity: EntityId, mode: FollowMode) -> StoreResult<()> {
        sqlx::query("insert into follows (account, entity_id, mode) values ($1, $2, $3) on conflict (account, entity_id) do update set mode = excluded.mode")
            .bind(account)
            .bind(entity)
            .bind(mode.as_str())
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn follows_of_entity(&self, entity: EntityId) -> StoreResult<Vec<Follow>> {
        let rows = sqlx::query("select account, entity_id, mode from follows where entity_id = $1 order by account").bind(entity).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(follow_of).collect()
    }

    async fn follows_of_account(&self, account: AccountId) -> StoreResult<Vec<Follow>> {
        let rows = sqlx::query("select account, entity_id, mode from follows where account = $1 order by entity_id").bind(account).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(follow_of).collect()
    }

    async fn put_label(&self, label: &Label) -> StoreResult<()> {
        sqlx::query(
            "insert into labels (address, class, name, source) values ($1, $2, $3, $4) on conflict (address) do update set class = excluded.class, name = excluded.name, source = excluded.source",
        )
        .bind(&label.address)
        .bind(label.class.as_str())
        .bind(&label.name)
        .bind(&label.source)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn delete_label(&self, address: &str) -> StoreResult<bool> {
        let done = sqlx::query("delete from labels where address = $1").bind(address).execute(&self.pool).await.map_err(db)?;
        Ok(done.rows_affected() == 1)
    }

    async fn labels(&self) -> StoreResult<Vec<Label>> {
        let rows = sqlx::query("select address, class, name, source from labels order by address").fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(label_of).collect()
    }

    async fn insert_trade(&self, trade: &Trade) -> StoreResult<bool> {
        let done = sqlx::query(
            "insert into trades (signature, wallet, mint, entity_id, slot, block_time, seen_at, side, token_amount_raw, decimals, token_program, quote, quote_amount_raw, venue, programs, fingerprint)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) on conflict do nothing",
        )
        .bind(&trade.signature)
        .bind(&trade.wallet)
        .bind(&trade.mint)
        .bind(trade.entity_id)
        .bind(i64_of(trade.slot))
        .bind(trade.block_time)
        .bind(trade.seen_at)
        .bind(trade.side.as_str())
        .bind(trade.token_amount_raw.to_string())
        .bind(trade.decimals as i32)
        .bind(trade.token_program.map(|p| p.as_str()))
        .bind(trade.quote.as_str())
        .bind(i64_of(trade.quote_amount_raw))
        .bind(trade.venue.as_str())
        .bind(serde_json::json!(trade.programs))
        .bind(serde_json::to_value(&trade.fingerprint).map_err(db)?)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(done.rows_affected() == 1)
    }

    async fn trades_of_entity(&self, entity: EntityId, limit: i64) -> StoreResult<Vec<Trade>> {
        let rows = sqlx::query(
            "select t.* from trades t join wallets w on w.address = t.wallet
             where w.entity_id = $1 and w.state = 'linked' order by t.slot desc, t.seen_at desc limit $2",
        )
        .bind(entity)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter().map(trade_of).collect()
    }

    async fn trades_of_wallet(&self, address: &str, limit: i64) -> StoreResult<Vec<Trade>> {
        let rows = sqlx::query("select * from trades where wallet = $1 order by slot desc, seen_at desc limit $2").bind(address).bind(limit.max(0)).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(trade_of).collect()
    }

    async fn insert_transfer(&self, transfer: &Transfer) -> StoreResult<bool> {
        let done = sqlx::query(
            "insert into transfers (signature, wallet, counterparty, mint, direction, slot, block_time, amount_raw, decimals)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) on conflict do nothing",
        )
        .bind(&transfer.signature)
        .bind(&transfer.wallet)
        .bind(&transfer.counterparty)
        .bind(transfer.mint.as_deref().unwrap_or(""))
        .bind(transfer.direction.as_str())
        .bind(i64_of(transfer.slot))
        .bind(transfer.block_time)
        .bind(transfer.amount_raw.to_string())
        .bind(transfer.decimals as i32)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(done.rows_affected() == 1)
    }

    async fn transfers_of_wallet(&self, address: &str, limit: i64) -> StoreResult<Vec<Transfer>> {
        let rows = sqlx::query("select * from transfers where wallet = $1 order by slot desc limit $2").bind(address).bind(limit.max(0)).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(transfer_of).collect()
    }

    async fn open_signal(&self, entity: EntityId, mint: &str, side: Side, since: i64) -> StoreResult<Option<Signal>> {
        let row = sqlx::query("select * from signals where entity_id = $1 and mint = $2 and side = $3 and last_at >= $4 order by last_at desc, id desc limit 1")
            .bind(entity)
            .bind(mint)
            .bind(side.as_str())
            .bind(since)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?;
        row.as_ref().map(signal_of).transpose()
    }

    async fn create_signal(&self, signal: &Signal) -> StoreResult<Signal> {
        let row = sqlx::query(
            "insert into signals (entity_id, mint, side, first_at, last_at, quote, quote_amount_raw, token_amount_raw, decimals, wallets, trades, venue)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) returning *",
        )
        .bind(signal.entity_id)
        .bind(&signal.mint)
        .bind(signal.side.as_str())
        .bind(signal.first_at)
        .bind(signal.last_at)
        .bind(signal.quote.as_str())
        .bind(i64_of(signal.quote_amount_raw))
        .bind(signal.token_amount_raw.to_string())
        .bind(signal.decimals as i32)
        .bind(serde_json::json!(signal.wallets))
        .bind(signal.trades as i32)
        .bind(signal.venue.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        signal_of(&row)
    }

    async fn update_signal(&self, signal: &Signal) -> StoreResult<()> {
        sqlx::query("update signals set last_at = $2, quote_amount_raw = $3, token_amount_raw = $4, wallets = $5, trades = $6, venue = $7 where id = $1")
            .bind(signal.id)
            .bind(signal.last_at)
            .bind(i64_of(signal.quote_amount_raw))
            .bind(signal.token_amount_raw.to_string())
            .bind(serde_json::json!(signal.wallets))
            .bind(signal.trades as i32)
            .bind(signal.venue.as_str())
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn set_signal_message(&self, signal: i64, account: AccountId, message_id: i64) -> StoreResult<()> {
        sqlx::query("insert into signal_messages (signal_id, account, message_id) values ($1, $2, $3) on conflict (signal_id, account) do update set message_id = excluded.message_id")
            .bind(signal)
            .bind(account)
            .bind(message_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn signal_messages(&self, signal: i64) -> StoreResult<Vec<(AccountId, i64)>> {
        let rows = sqlx::query("select account, message_id from signal_messages where signal_id = $1 order by account").bind(signal).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(|r| Ok((r.try_get("account").map_err(db)?, r.try_get("message_id").map_err(db)?))).collect()
    }

    async fn get_state(&self, key: &str) -> StoreResult<Option<String>> {
        let row = sqlx::query("select value from state where key = $1").bind(key).fetch_optional(&self.pool).await.map_err(db)?;
        row.map(|r| r.try_get("value").map_err(db)).transpose()
    }

    async fn set_state(&self, key: &str, value: &str) -> StoreResult<()> {
        sqlx::query("insert into state (key, value) values ($1, $2) on conflict (key) do update set value = excluded.value").bind(key).bind(value).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    async fn add_credits(&self, day: &str, lane: &str, credits: i64) -> StoreResult<()> {
        sqlx::query("insert into budget_ledger (day, lane, credits) values ($1, $2, $3) on conflict (day, lane) do update set credits = budget_ledger.credits + excluded.credits")
            .bind(day)
            .bind(lane)
            .bind(credits)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn credits(&self, day: &str) -> StoreResult<Vec<(String, i64)>> {
        let rows = sqlx::query("select lane, credits from budget_ledger where day = $1 order by lane").bind(day).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(|r| Ok((r.try_get("lane").map_err(db)?, r.try_get("credits").map_err(db)?))).collect()
    }

    async fn counts(&self) -> StoreResult<Counts> {
        let row = sqlx::query(
            "select (select count(*) from entities) as entities, (select count(*) from wallets) as wallets, (select count(*) from trades) as trades, (select count(*) from transfers) as transfers",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(Counts {
            entities: row.try_get("entities").map_err(db)?,
            wallets: row.try_get("wallets").map_err(db)?,
            trades: row.try_get("trades").map_err(db)?,
            transfers: row.try_get("transfers").map_err(db)?,
        })
    }

    async fn void_evidence_row(&self, id: i64) -> StoreResult<bool> {
        let done = sqlx::query("update evidence set voided = true where id = $1 and not voided").bind(id).execute(&self.pool).await.map_err(db)?;
        Ok(done.rows_affected() == 1)
    }

    async fn repoint_evidence(&self, kind: &str, points: i32) -> StoreResult<u64> {
        let done = sqlx::query("update evidence set points = $2 where kind = $1 and not voided and points <> $2").bind(kind).bind(points).execute(&self.pool).await.map_err(db)?;
        Ok(done.rows_affected())
    }

    async fn evidence_of_entity(&self, entity: EntityId) -> StoreResult<Vec<Evidence>> {
        let rows = sqlx::query("select id, wallet, entity_id, kind, family, points, signatures, details, source, voided, observed_at from evidence where entity_id = $1 order by id desc")
            .bind(entity)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.iter().map(evidence_of).collect()
    }

    async fn move_evidence(&self, from: EntityId, into: EntityId) -> StoreResult<u64> {
        let done = sqlx::query("update evidence set entity_id = $2 where entity_id = $1").bind(from).bind(into).execute(&self.pool).await.map_err(db)?;
        Ok(done.rows_affected())
    }

    async fn wallets_in_state(&self, state: WalletState) -> StoreResult<Vec<Wallet>> {
        let rows = sqlx::query("select address, entity_id, state, score, linked_by, first_seen, last_signature from wallets where state = $1 order by address")
            .bind(state.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.iter().map(wallet_of).collect()
    }

    async fn set_entity_flags(&self, id: EntityId, copyable: Option<bool>, bait: Option<bool>) -> StoreResult<()> {
        sqlx::query("update entities set copyable = coalesce($2, copyable), bait = coalesce($3, bait) where id = $1").bind(id).bind(copyable).bind(bait).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    async fn trades_of_mint(&self, mint: &str, since: i64, limit: i64) -> StoreResult<Vec<Trade>> {
        let rows = sqlx::query("select * from trades where mint = $1 and coalesce(block_time, seen_at) >= $2 order by coalesce(block_time, seen_at), slot limit $3")
            .bind(mint)
            .bind(since)
            .bind(limit.max(0))
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.iter().map(trade_of).collect()
    }

    async fn signals_since(&self, since: i64) -> StoreResult<Vec<Signal>> {
        let rows = sqlx::query("select * from signals where last_at >= $1 order by first_at, id").bind(since).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(signal_of).collect()
    }

    async fn signal(&self, id: i64) -> StoreResult<Option<Signal>> {
        let row = sqlx::query("select * from signals where id = $1").bind(id).fetch_optional(&self.pool).await.map_err(db)?;
        row.as_ref().map(signal_of).transpose()
    }

    async fn put_deposit(&self, deposit: &DepositAddr) -> StoreResult<()> {
        sqlx::query(
            "insert into deposit_addrs (address, entity_id, exchange, status, senders, first_seen) values ($1, $2, $3, $4, $5, $6)
             on conflict (address) do update set entity_id = excluded.entity_id, exchange = excluded.exchange, status = excluded.status, senders = excluded.senders",
        )
        .bind(&deposit.address)
        .bind(deposit.entity_id)
        .bind(&deposit.exchange)
        .bind(deposit.status.as_str())
        .bind(deposit.senders)
        .bind(deposit.first_seen)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn deposit(&self, address: &str) -> StoreResult<Option<DepositAddr>> {
        let row = sqlx::query("select * from deposit_addrs where address = $1").bind(address).fetch_optional(&self.pool).await.map_err(db)?;
        row.as_ref().map(deposit_of).transpose()
    }

    async fn deposits_of(&self, entity: EntityId) -> StoreResult<Vec<DepositAddr>> {
        let rows = sqlx::query("select * from deposit_addrs where entity_id = $1 order by address").bind(entity).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(deposit_of).collect()
    }

    async fn watched_deposits(&self) -> StoreResult<Vec<DepositAddr>> {
        let rows = sqlx::query("select d.* from deposit_addrs d join entities e on e.id = d.entity_id where d.status = 'qualified' and e.status = 'tracking' order by d.address")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.iter().map(deposit_of).collect()
    }

    async fn enqueue_job(&self, job: &NewJob, now: i64) -> StoreResult<Option<i64>> {
        let row = sqlx::query(
            "insert into trail_jobs (kind, subject, entity_id, payload, run_after, created_at) values ($1, $2, $3, $4, $5, $6)
             on conflict (kind, subject) where status in ('queued', 'running') do nothing returning id",
        )
        .bind(&job.kind)
        .bind(&job.subject)
        .bind(job.entity_id)
        .bind(&job.payload)
        .bind(job.run_after)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        row.map(|r| r.try_get("id").map_err(db)).transpose()
    }

    async fn claim_job(&self, now: i64) -> StoreResult<Option<Job>> {
        let row = sqlx::query(
            "update trail_jobs set status = 'running', attempts = attempts + 1
             where id = (select id from trail_jobs where status = 'queued' and run_after <= $1 order by run_after, id limit 1 for update skip locked)
             returning *",
        )
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        row.as_ref().map(job_of).transpose()
    }

    async fn finish_job(&self, id: i64, ok: bool) -> StoreResult<()> {
        sqlx::query("update trail_jobs set status = $2 where id = $1").bind(id).bind(if ok { "done" } else { "failed" }).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    async fn retry_job(&self, id: i64, run_after: i64, payload: &Value) -> StoreResult<()> {
        sqlx::query("update trail_jobs set status = 'queued', run_after = $2, payload = $3 where id = $1").bind(id).bind(run_after).bind(payload).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    async fn job_counts(&self) -> StoreResult<(i64, i64)> {
        let row =
            sqlx::query("select count(*) filter (where status = 'queued') as queued, count(*) filter (where status = 'running') as running from trail_jobs").fetch_one(&self.pool).await.map_err(db)?;
        Ok((row.try_get("queued").map_err(db)?, row.try_get("running").map_err(db)?))
    }

    async fn put_doc(&self, kind: &str, key: &str, value: &Value, now: i64) -> StoreResult<()> {
        sqlx::query("insert into docs (kind, key, value, updated_at) values ($1, $2, $3, $4) on conflict (kind, key) do update set value = excluded.value, updated_at = excluded.updated_at")
            .bind(kind)
            .bind(key)
            .bind(value)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn doc(&self, kind: &str, key: &str) -> StoreResult<Option<Value>> {
        let row = sqlx::query("select value from docs where kind = $1 and key = $2").bind(kind).bind(key).fetch_optional(&self.pool).await.map_err(db)?;
        row.map(|r| r.try_get("value").map_err(db)).transpose()
    }

    async fn docs(&self, kind: &str, prefix: &str, limit: i64) -> StoreResult<Vec<(String, Value)>> {
        // `left` and not `like`: an address has no wildcard characters, but a prefix is still data.
        let rows = sqlx::query("select key, value from docs where kind = $1 and left(key, length($2)) = $2 order by key limit $3")
            .bind(kind)
            .bind(prefix)
            .bind(limit.max(0))
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.iter().map(|r| Ok((r.try_get("key").map_err(db)?, r.try_get("value").map_err(db)?))).collect()
    }

    async fn delete_doc(&self, kind: &str, key: &str) -> StoreResult<bool> {
        let done = sqlx::query("delete from docs where kind = $1 and key = $2").bind(kind).bind(key).execute(&self.pool).await.map_err(db)?;
        Ok(done.rows_affected() == 1)
    }

    async fn insert_position(&self, position: &Position) -> StoreResult<Option<i64>> {
        let row = sqlx::query(
            "insert into positions (account, entity_id, signal_id, mint, decimals, mode, status, entry_lamports, tokens_raw, tokens_left, exit_lamports, opened_at, closed_at, exit_reason, open_signature, note)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) on conflict (account, signal_id, mode) do nothing returning id",
        )
        .bind(position.account)
        .bind(position.entity_id)
        .bind(position.signal_id)
        .bind(&position.mint)
        .bind(position.decimals as i32)
        .bind(position.mode.as_str())
        .bind(position.status.as_str())
        .bind(i64_of(position.entry_lamports))
        .bind(position.tokens_raw.to_string())
        .bind(position.tokens_left.to_string())
        .bind(i64_of(position.exit_lamports))
        .bind(position.opened_at)
        .bind(position.closed_at)
        .bind(&position.exit_reason)
        .bind(&position.open_signature)
        .bind(&position.note)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        row.map(|r| r.try_get("id").map_err(db)).transpose()
    }

    async fn update_position(&self, position: &Position) -> StoreResult<()> {
        sqlx::query(
            "update positions set status = $2, entry_lamports = $3, tokens_raw = $4, tokens_left = $5, exit_lamports = $6, closed_at = $7, exit_reason = $8, open_signature = $9, note = $10 where id = $1",
        )
        .bind(position.id)
        .bind(position.status.as_str())
        .bind(i64_of(position.entry_lamports))
        .bind(position.tokens_raw.to_string())
        .bind(position.tokens_left.to_string())
        .bind(i64_of(position.exit_lamports))
        .bind(position.closed_at)
        .bind(&position.exit_reason)
        .bind(&position.open_signature)
        .bind(&position.note)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn position(&self, id: i64) -> StoreResult<Option<Position>> {
        let row = sqlx::query("select * from positions where id = $1").bind(id).fetch_optional(&self.pool).await.map_err(db)?;
        row.as_ref().map(position_of).transpose()
    }

    async fn open_positions(&self) -> StoreResult<Vec<Position>> {
        let rows = sqlx::query("select * from positions where status in ('opening', 'open', 'unconfirmed') order by id").fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(position_of).collect()
    }

    async fn positions_of_account(&self, account: AccountId, since: i64) -> StoreResult<Vec<Position>> {
        let rows = sqlx::query("select * from positions where account = $1 and opened_at >= $2 order by opened_at desc, id desc").bind(account).bind(since).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(position_of).collect()
    }

    async fn positions_of_entity(&self, entity: EntityId, mode: PositionMode) -> StoreResult<Vec<Position>> {
        let rows = sqlx::query("select * from positions where entity_id = $1 and mode = $2 order by id").bind(entity).bind(mode.as_str()).fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(position_of).collect()
    }

    async fn acquire_lease(&self, name: &str, holder: &str, ttl_s: i64, now: i64) -> StoreResult<bool> {
        // One statement, so two processes cannot both take a free lease.
        let done = sqlx::query(
            "insert into state (key, value) values ($1, $2) on conflict (key) do update set value = excluded.value
             where split_part(state.value, '|', 1) = $3 or split_part(state.value, '|', 2)::bigint < $4",
        )
        .bind(format!("lease:{name}"))
        .bind(format!("{holder}|{}", now + ttl_s))
        .bind(holder)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(done.rows_affected() == 1)
    }
}

fn deposit_of(row: &PgRow) -> StoreResult<DepositAddr> {
    let status: String = row.try_get("status").map_err(db)?;
    Ok(DepositAddr {
        address: row.try_get("address").map_err(db)?,
        entity_id: row.try_get("entity_id").map_err(db)?,
        exchange: row.try_get("exchange").map_err(db)?,
        status: DepositStatus::parse(&status).ok_or_else(|| bad("deposit_addrs.status", &status))?,
        senders: row.try_get("senders").map_err(db)?,
        first_seen: row.try_get("first_seen").map_err(db)?,
    })
}

fn job_of(row: &PgRow) -> StoreResult<Job> {
    let status: String = row.try_get("status").map_err(db)?;
    Ok(Job {
        id: row.try_get("id").map_err(db)?,
        kind: row.try_get("kind").map_err(db)?,
        subject: row.try_get("subject").map_err(db)?,
        entity_id: row.try_get("entity_id").map_err(db)?,
        payload: row.try_get("payload").map_err(db)?,
        status: JobStatus::parse(&status).ok_or_else(|| bad("trail_jobs.status", &status))?,
        attempts: row.try_get("attempts").map_err(db)?,
        run_after: row.try_get("run_after").map_err(db)?,
        created_at: row.try_get("created_at").map_err(db)?,
    })
}

fn position_of(row: &PgRow) -> StoreResult<Position> {
    let mode: String = row.try_get("mode").map_err(db)?;
    let status: String = row.try_get("status").map_err(db)?;
    Ok(Position {
        id: row.try_get("id").map_err(db)?,
        account: row.try_get("account").map_err(db)?,
        entity_id: row.try_get("entity_id").map_err(db)?,
        signal_id: row.try_get("signal_id").map_err(db)?,
        mint: row.try_get("mint").map_err(db)?,
        decimals: row.try_get::<i32, _>("decimals").map_err(db)? as u8,
        mode: PositionMode::parse(&mode).ok_or_else(|| bad("positions.mode", &mode))?,
        status: PositionStatus::parse(&status).ok_or_else(|| bad("positions.status", &status))?,
        entry_lamports: row.try_get::<i64, _>("entry_lamports").map_err(db)? as u64,
        tokens_raw: u64_of("positions.tokens_raw", row.try_get("tokens_raw").map_err(db)?)?,
        tokens_left: u64_of("positions.tokens_left", row.try_get("tokens_left").map_err(db)?)?,
        exit_lamports: row.try_get::<i64, _>("exit_lamports").map_err(db)? as u64,
        opened_at: row.try_get("opened_at").map_err(db)?,
        closed_at: row.try_get("closed_at").map_err(db)?,
        exit_reason: row.try_get("exit_reason").map_err(db)?,
        open_signature: row.try_get("open_signature").map_err(db)?,
        note: row.try_get("note").map_err(db)?,
    })
}

fn follow_of(row: &PgRow) -> StoreResult<Follow> {
    let mode: String = row.try_get("mode").map_err(db)?;
    Ok(Follow { account: row.try_get("account").map_err(db)?, entity_id: row.try_get("entity_id").map_err(db)?, mode: FollowMode::parse(&mode).ok_or_else(|| bad("follows.mode", &mode))? })
}
