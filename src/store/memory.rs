//! The in-memory store: what tests run against, and what the binary falls
//! back to (with a loud warning) when no database is configured.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use parking_lot::Mutex;

use super::{Counts, Store, StoreResult};
use crate::model::*;

#[derive(Default)]
struct Inner {
    accounts: BTreeMap<AccountId, Account>,
    invites: HashMap<String, i32>,
    entities: BTreeMap<EntityId, Entity>,
    next_entity: EntityId,
    wallets: BTreeMap<String, Wallet>,
    evidence: Vec<Evidence>,
    follows: BTreeMap<(AccountId, EntityId), FollowMode>,
    labels: BTreeMap<String, Label>,
    trades: Vec<Trade>,
    transfers: Vec<Transfer>,
    signals: Vec<Signal>,
    signal_messages: BTreeMap<(i64, AccountId), i64>,
    state: HashMap<String, String>,
    credits: BTreeMap<(String, String), i64>,
    deposits: BTreeMap<String, DepositAddr>,
    jobs: Vec<Job>,
    docs: BTreeMap<(String, String), serde_json::Value>,
    positions: Vec<Position>,
}

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore::default()
    }
}

/// Newest first: by slot, then by the order the trades arrived.
fn newest_first(mut rows: Vec<(usize, Trade)>, limit: i64) -> Vec<Trade> {
    rows.sort_by_key(|row| std::cmp::Reverse((row.1.slot, row.0)));
    rows.into_iter().take(limit.max(0) as usize).map(|(_, t)| t).collect()
}

#[async_trait]
impl Store for MemoryStore {
    async fn upsert_account(&self, id: AccountId, name: &str, is_admin: bool, now: i64) -> StoreResult<Account> {
        let mut inner = self.inner.lock();
        let account = inner.accounts.entry(id).or_insert_with(|| Account { id, name: String::new(), is_admin, paused: false, created_at: now });
        account.name = name.to_string();
        account.is_admin = is_admin;
        Ok(account.clone())
    }

    async fn account(&self, id: AccountId) -> StoreResult<Option<Account>> {
        Ok(self.inner.lock().accounts.get(&id).cloned())
    }

    async fn accounts(&self) -> StoreResult<Vec<Account>> {
        let mut out: Vec<Account> = self.inner.lock().accounts.values().cloned().collect();
        out.sort_by_key(|a| (a.created_at, a.id));
        Ok(out)
    }

    async fn set_paused(&self, id: AccountId, paused: bool) -> StoreResult<()> {
        if let Some(account) = self.inner.lock().accounts.get_mut(&id) {
            account.paused = paused;
        }
        Ok(())
    }

    async fn create_invite(&self, code: &str, uses: i32, _created_by: AccountId, _now: i64) -> StoreResult<()> {
        self.inner.lock().invites.insert(code.to_string(), uses);
        Ok(())
    }

    async fn redeem_invite(&self, code: &str) -> StoreResult<bool> {
        let mut inner = self.inner.lock();
        match inner.invites.get_mut(code) {
            Some(left) if *left > 0 => {
                *left -= 1;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn create_entity(&self, label: &str, created_by: AccountId, now: i64) -> StoreResult<Entity> {
        let mut inner = self.inner.lock();
        inner.next_entity += 1;
        let entity = Entity { id: inner.next_entity, label: label.to_string(), status: EntityStatus::Tracking, copyable: false, bait: false, created_by, created_at: now };
        inner.entities.insert(entity.id, entity.clone());
        Ok(entity)
    }

    async fn entity(&self, id: EntityId) -> StoreResult<Option<Entity>> {
        Ok(self.inner.lock().entities.get(&id).cloned())
    }

    async fn entities(&self) -> StoreResult<Vec<Entity>> {
        Ok(self.inner.lock().entities.values().cloned().collect())
    }

    async fn rename_entity(&self, id: EntityId, label: &str) -> StoreResult<bool> {
        Ok(self.inner.lock().entities.get_mut(&id).map(|e| e.label = label.to_string()).is_some())
    }

    async fn set_entity_status(&self, id: EntityId, status: EntityStatus) -> StoreResult<bool> {
        Ok(self.inner.lock().entities.get_mut(&id).map(|e| e.status = status).is_some())
    }

    async fn put_wallet(&self, wallet: &Wallet) -> StoreResult<()> {
        let mut inner = self.inner.lock();
        match inner.wallets.get_mut(&wallet.address) {
            Some(existing) => {
                existing.entity_id = wallet.entity_id;
                existing.state = wallet.state;
                existing.score = wallet.score;
                existing.linked_by = wallet.linked_by;
            }
            None => {
                let mut fresh = wallet.clone();
                fresh.last_signature = None;
                inner.wallets.insert(fresh.address.clone(), fresh);
            }
        }
        Ok(())
    }

    async fn wallet(&self, address: &str) -> StoreResult<Option<Wallet>> {
        Ok(self.inner.lock().wallets.get(address).cloned())
    }

    async fn wallets_of(&self, entity: EntityId) -> StoreResult<Vec<Wallet>> {
        let mut out: Vec<Wallet> = self.inner.lock().wallets.values().filter(|w| w.entity_id == Some(entity)).cloned().collect();
        out.sort_by(|a, b| (a.first_seen, &a.address).cmp(&(b.first_seen, &b.address)));
        Ok(out)
    }

    async fn watched_wallets(&self) -> StoreResult<Vec<Wallet>> {
        let inner = self.inner.lock();
        Ok(inner
            .wallets
            .values()
            .filter(|w| matches!(w.state, WalletState::Linked | WalletState::Candidate))
            .filter(|w| w.entity_id.and_then(|id| inner.entities.get(&id)).is_some_and(|e| e.status == EntityStatus::Tracking))
            .cloned()
            .collect())
    }

    async fn set_cursor(&self, address: &str, signature: &str) -> StoreResult<()> {
        if let Some(wallet) = self.inner.lock().wallets.get_mut(address) {
            wallet.last_signature = Some(signature.to_string());
        }
        Ok(())
    }

    async fn add_evidence(&self, evidence: &NewEvidence) -> StoreResult<i64> {
        let mut inner = self.inner.lock();
        let id = inner.evidence.len() as i64 + 1;
        inner.evidence.push(Evidence {
            id,
            wallet: evidence.wallet.clone(),
            entity_id: evidence.entity_id,
            kind: evidence.kind.clone(),
            family: evidence.family.clone(),
            points: evidence.points,
            signatures: evidence.signatures.clone(),
            details: evidence.details.clone(),
            source: evidence.source.clone(),
            voided: false,
            observed_at: evidence.observed_at,
        });
        Ok(id)
    }

    async fn evidence_of_wallet(&self, address: &str) -> StoreResult<Vec<Evidence>> {
        let mut out: Vec<Evidence> = self.inner.lock().evidence.iter().filter(|e| e.wallet == address).cloned().collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.id));
        Ok(out)
    }

    async fn void_evidence(&self, address: &str, entity: EntityId) -> StoreResult<u64> {
        let mut inner = self.inner.lock();
        let mut voided = 0;
        for row in inner.evidence.iter_mut().filter(|e| e.wallet == address && e.entity_id == entity && !e.voided) {
            row.voided = true;
            voided += 1;
        }
        Ok(voided)
    }

    async fn set_follow(&self, account: AccountId, entity: EntityId, mode: FollowMode) -> StoreResult<()> {
        self.inner.lock().follows.insert((account, entity), mode);
        Ok(())
    }

    async fn follows_of_entity(&self, entity: EntityId) -> StoreResult<Vec<Follow>> {
        Ok(self.inner.lock().follows.iter().filter(|((_, e), _)| *e == entity).map(|((a, e), m)| Follow { account: *a, entity_id: *e, mode: *m }).collect())
    }

    async fn follows_of_account(&self, account: AccountId) -> StoreResult<Vec<Follow>> {
        Ok(self.inner.lock().follows.iter().filter(|((a, _), _)| *a == account).map(|((a, e), m)| Follow { account: *a, entity_id: *e, mode: *m }).collect())
    }

    async fn put_label(&self, label: &Label) -> StoreResult<()> {
        self.inner.lock().labels.insert(label.address.clone(), label.clone());
        Ok(())
    }

    async fn delete_label(&self, address: &str) -> StoreResult<bool> {
        Ok(self.inner.lock().labels.remove(address).is_some())
    }

    async fn labels(&self) -> StoreResult<Vec<Label>> {
        Ok(self.inner.lock().labels.values().cloned().collect())
    }

    async fn insert_trade(&self, trade: &Trade) -> StoreResult<bool> {
        let mut inner = self.inner.lock();
        if inner.trades.iter().any(|t| t.signature == trade.signature && t.wallet == trade.wallet && t.mint == trade.mint) {
            return Ok(false);
        }
        inner.trades.push(trade.clone());
        Ok(true)
    }

    async fn trades_of_entity(&self, entity: EntityId, limit: i64) -> StoreResult<Vec<Trade>> {
        let inner = self.inner.lock();
        let linked: Vec<&str> = inner.wallets.values().filter(|w| w.entity_id == Some(entity) && w.state == WalletState::Linked).map(|w| w.address.as_str()).collect();
        let rows = inner.trades.iter().enumerate().filter(|(_, t)| linked.contains(&t.wallet.as_str())).map(|(i, t)| (i, t.clone())).collect();
        Ok(newest_first(rows, limit))
    }

    async fn trades_of_wallet(&self, address: &str, limit: i64) -> StoreResult<Vec<Trade>> {
        let inner = self.inner.lock();
        let rows = inner.trades.iter().enumerate().filter(|(_, t)| t.wallet == address).map(|(i, t)| (i, t.clone())).collect();
        Ok(newest_first(rows, limit))
    }

    async fn insert_transfer(&self, transfer: &Transfer) -> StoreResult<bool> {
        let mut inner = self.inner.lock();
        let same =
            |t: &Transfer| t.signature == transfer.signature && t.wallet == transfer.wallet && t.counterparty == transfer.counterparty && t.mint == transfer.mint && t.direction == transfer.direction;
        if inner.transfers.iter().any(same) {
            return Ok(false);
        }
        inner.transfers.push(transfer.clone());
        Ok(true)
    }

    async fn transfers_of_wallet(&self, address: &str, limit: i64) -> StoreResult<Vec<Transfer>> {
        let inner = self.inner.lock();
        let mut rows: Vec<(usize, Transfer)> = inner.transfers.iter().enumerate().filter(|(_, t)| t.wallet == address).map(|(i, t)| (i, t.clone())).collect();
        rows.sort_by_key(|row| std::cmp::Reverse((row.1.slot, row.0)));
        Ok(rows.into_iter().take(limit.max(0) as usize).map(|(_, t)| t).collect())
    }

    async fn open_signal(&self, entity: EntityId, mint: &str, side: Side, since: i64) -> StoreResult<Option<Signal>> {
        let inner = self.inner.lock();
        Ok(inner.signals.iter().filter(|s| s.entity_id == entity && s.mint == mint && s.side == side && s.last_at >= since).max_by_key(|s| (s.last_at, s.id)).cloned())
    }

    async fn create_signal(&self, signal: &Signal) -> StoreResult<Signal> {
        let mut inner = self.inner.lock();
        let mut stored = signal.clone();
        stored.id = inner.signals.len() as i64 + 1;
        inner.signals.push(stored.clone());
        Ok(stored)
    }

    async fn update_signal(&self, signal: &Signal) -> StoreResult<()> {
        if let Some(existing) = self.inner.lock().signals.iter_mut().find(|s| s.id == signal.id) {
            *existing = signal.clone();
        }
        Ok(())
    }

    async fn set_signal_message(&self, signal: i64, account: AccountId, message_id: i64) -> StoreResult<()> {
        self.inner.lock().signal_messages.insert((signal, account), message_id);
        Ok(())
    }

    async fn signal_messages(&self, signal: i64) -> StoreResult<Vec<(AccountId, i64)>> {
        Ok(self.inner.lock().signal_messages.iter().filter(|((s, _), _)| *s == signal).map(|((_, a), m)| (*a, *m)).collect())
    }

    async fn get_state(&self, key: &str) -> StoreResult<Option<String>> {
        Ok(self.inner.lock().state.get(key).cloned())
    }

    async fn set_state(&self, key: &str, value: &str) -> StoreResult<()> {
        self.inner.lock().state.insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn add_credits(&self, day: &str, lane: &str, credits: i64) -> StoreResult<()> {
        *self.inner.lock().credits.entry((day.to_string(), lane.to_string())).or_insert(0) += credits;
        Ok(())
    }

    async fn credits(&self, day: &str) -> StoreResult<Vec<(String, i64)>> {
        Ok(self.inner.lock().credits.iter().filter(|((d, _), _)| d == day).map(|((_, lane), n)| (lane.clone(), *n)).collect())
    }

    async fn counts(&self) -> StoreResult<Counts> {
        let inner = self.inner.lock();
        Ok(Counts { entities: inner.entities.len() as i64, wallets: inner.wallets.len() as i64, trades: inner.trades.len() as i64, transfers: inner.transfers.len() as i64 })
    }

    async fn void_evidence_row(&self, id: i64) -> StoreResult<bool> {
        let mut inner = self.inner.lock();
        match inner.evidence.iter_mut().find(|e| e.id == id && !e.voided) {
            Some(row) => {
                row.voided = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn repoint_evidence(&self, kind: &str, points: i32) -> StoreResult<u64> {
        let mut changed = 0;
        for row in self.inner.lock().evidence.iter_mut().filter(|e| e.kind == kind && !e.voided && e.points != points) {
            row.points = points;
            changed += 1;
        }
        Ok(changed)
    }

    async fn evidence_of_entity(&self, entity: EntityId) -> StoreResult<Vec<Evidence>> {
        let mut rows: Vec<Evidence> = self.inner.lock().evidence.iter().filter(|e| e.entity_id == entity).cloned().collect();
        rows.reverse();
        Ok(rows)
    }

    async fn move_evidence(&self, from: EntityId, into: EntityId) -> StoreResult<u64> {
        let mut moved = 0;
        for row in self.inner.lock().evidence.iter_mut().filter(|e| e.entity_id == from) {
            row.entity_id = into;
            moved += 1;
        }
        Ok(moved)
    }

    async fn wallets_in_state(&self, state: WalletState) -> StoreResult<Vec<Wallet>> {
        Ok(self.inner.lock().wallets.values().filter(|w| w.state == state).cloned().collect())
    }

    async fn set_entity_flags(&self, id: EntityId, copyable: Option<bool>, bait: Option<bool>) -> StoreResult<()> {
        if let Some(entity) = self.inner.lock().entities.get_mut(&id) {
            entity.copyable = copyable.unwrap_or(entity.copyable);
            entity.bait = bait.unwrap_or(entity.bait);
        }
        Ok(())
    }

    async fn trades_of_mint(&self, mint: &str, since: i64, limit: i64) -> StoreResult<Vec<Trade>> {
        let mut rows: Vec<Trade> = self.inner.lock().trades.iter().filter(|t| t.mint == mint && t.block_time.unwrap_or(t.seen_at) >= since).cloned().collect();
        rows.sort_by_key(|t| (t.block_time.unwrap_or(t.seen_at), t.slot));
        rows.truncate(limit.max(0) as usize);
        Ok(rows)
    }

    async fn signals_since(&self, since: i64) -> StoreResult<Vec<Signal>> {
        let mut rows: Vec<Signal> = self.inner.lock().signals.iter().filter(|s| s.last_at >= since).cloned().collect();
        rows.sort_by_key(|s| (s.first_at, s.id));
        Ok(rows)
    }

    async fn signal(&self, id: i64) -> StoreResult<Option<Signal>> {
        Ok(self.inner.lock().signals.iter().find(|s| s.id == id).cloned())
    }

    async fn put_deposit(&self, deposit: &DepositAddr) -> StoreResult<()> {
        let mut inner = self.inner.lock();
        let first_seen = inner.deposits.get(&deposit.address).map(|d| d.first_seen).unwrap_or(deposit.first_seen);
        inner.deposits.insert(deposit.address.clone(), DepositAddr { first_seen, ..deposit.clone() });
        Ok(())
    }

    async fn deposit(&self, address: &str) -> StoreResult<Option<DepositAddr>> {
        Ok(self.inner.lock().deposits.get(address).cloned())
    }

    async fn deposits_of(&self, entity: EntityId) -> StoreResult<Vec<DepositAddr>> {
        Ok(self.inner.lock().deposits.values().filter(|d| d.entity_id == entity).cloned().collect())
    }

    async fn watched_deposits(&self) -> StoreResult<Vec<DepositAddr>> {
        let inner = self.inner.lock();
        Ok(inner.deposits.values().filter(|d| d.status == DepositStatus::Qualified && inner.entities.get(&d.entity_id).is_some_and(|e| e.status == EntityStatus::Tracking)).cloned().collect())
    }

    async fn enqueue_job(&self, job: &NewJob, now: i64) -> StoreResult<Option<i64>> {
        let mut inner = self.inner.lock();
        if inner.jobs.iter().any(|j| j.kind == job.kind && j.subject == job.subject && matches!(j.status, JobStatus::Queued | JobStatus::Running)) {
            return Ok(None);
        }
        let id = inner.jobs.len() as i64 + 1;
        inner.jobs.push(Job {
            id,
            kind: job.kind.clone(),
            subject: job.subject.clone(),
            entity_id: job.entity_id,
            payload: job.payload.clone(),
            status: JobStatus::Queued,
            attempts: 0,
            run_after: job.run_after,
            created_at: now,
        });
        Ok(Some(id))
    }

    async fn claim_job(&self, now: i64) -> StoreResult<Option<Job>> {
        let mut inner = self.inner.lock();
        let Some(job) = inner.jobs.iter_mut().filter(|j| j.status == JobStatus::Queued && j.run_after <= now).min_by_key(|j| (j.run_after, j.id)) else { return Ok(None) };
        job.status = JobStatus::Running;
        job.attempts += 1;
        Ok(Some(job.clone()))
    }

    async fn finish_job(&self, id: i64, ok: bool) -> StoreResult<()> {
        if let Some(job) = self.inner.lock().jobs.iter_mut().find(|j| j.id == id) {
            job.status = if ok { JobStatus::Done } else { JobStatus::Failed };
        }
        Ok(())
    }

    async fn retry_job(&self, id: i64, run_after: i64, payload: &serde_json::Value) -> StoreResult<()> {
        if let Some(job) = self.inner.lock().jobs.iter_mut().find(|j| j.id == id) {
            job.status = JobStatus::Queued;
            job.run_after = run_after;
            job.payload = payload.clone();
        }
        Ok(())
    }

    async fn job_counts(&self) -> StoreResult<(i64, i64)> {
        let inner = self.inner.lock();
        let count = |status: JobStatus| inner.jobs.iter().filter(|j| j.status == status).count() as i64;
        Ok((count(JobStatus::Queued), count(JobStatus::Running)))
    }

    async fn put_doc(&self, kind: &str, key: &str, value: &serde_json::Value, _now: i64) -> StoreResult<()> {
        self.inner.lock().docs.insert((kind.to_string(), key.to_string()), value.clone());
        Ok(())
    }

    async fn doc(&self, kind: &str, key: &str) -> StoreResult<Option<serde_json::Value>> {
        Ok(self.inner.lock().docs.get(&(kind.to_string(), key.to_string())).cloned())
    }

    async fn docs(&self, kind: &str, prefix: &str, limit: i64) -> StoreResult<Vec<(String, serde_json::Value)>> {
        Ok(self.inner.lock().docs.iter().filter(|((k, key), _)| k == kind && key.starts_with(prefix)).take(limit.max(0) as usize).map(|((_, key), value)| (key.clone(), value.clone())).collect())
    }

    async fn delete_doc(&self, kind: &str, key: &str) -> StoreResult<bool> {
        Ok(self.inner.lock().docs.remove(&(kind.to_string(), key.to_string())).is_some())
    }

    async fn insert_position(&self, position: &Position) -> StoreResult<Option<i64>> {
        let mut inner = self.inner.lock();
        if inner.positions.iter().any(|p| p.account == position.account && p.signal_id == position.signal_id && p.mode == position.mode) {
            return Ok(None);
        }
        let id = inner.positions.len() as i64 + 1;
        inner.positions.push(Position { id, ..position.clone() });
        Ok(Some(id))
    }

    async fn update_position(&self, position: &Position) -> StoreResult<()> {
        if let Some(row) = self.inner.lock().positions.iter_mut().find(|p| p.id == position.id) {
            *row = position.clone();
        }
        Ok(())
    }

    async fn position(&self, id: i64) -> StoreResult<Option<Position>> {
        Ok(self.inner.lock().positions.iter().find(|p| p.id == id).cloned())
    }

    async fn open_positions(&self) -> StoreResult<Vec<Position>> {
        Ok(self.inner.lock().positions.iter().filter(|p| p.is_open()).cloned().collect())
    }

    async fn positions_of_account(&self, account: AccountId, since: i64) -> StoreResult<Vec<Position>> {
        let mut rows: Vec<Position> = self.inner.lock().positions.iter().filter(|p| p.account == account && p.opened_at >= since).cloned().collect();
        rows.sort_by_key(|p| std::cmp::Reverse((p.opened_at, p.id)));
        Ok(rows)
    }

    async fn positions_of_entity(&self, entity: EntityId, mode: PositionMode) -> StoreResult<Vec<Position>> {
        Ok(self.inner.lock().positions.iter().filter(|p| p.entity_id == entity && p.mode == mode).cloned().collect())
    }

    async fn acquire_lease(&self, name: &str, holder: &str, ttl_s: i64, now: i64) -> StoreResult<bool> {
        let mut inner = self.inner.lock();
        let key = format!("lease:{name}");
        let current: Option<(String, i64)> = inner.state.get(&key).and_then(|v| v.split_once('|').and_then(|(h, until)| Some((h.to_string(), until.parse().ok()?))));
        if let Some((other, until)) = current
            && other != holder
            && until >= now
        {
            return Ok(false);
        }
        inner.state.insert(key, format!("{holder}|{}", now + ttl_s));
        Ok(true)
    }
}
