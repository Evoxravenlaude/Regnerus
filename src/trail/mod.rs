//! The trail engine: where a wallet's money came from and where it went.
//!
//! Work is queued as jobs so it runs on the forensics lane at its own
//! pace, waits when the day's allowance is spent, and survives a restart.

pub mod back;
pub mod exit;
pub mod walk;

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use crate::app::App;
use crate::chain::solana::rpc::{RpcError, SignatureInfo};
use crate::chain::solana::tx::{Classified, ParsedTx, TradeCore, classify};
use crate::model::*;
use crate::obs::budget::Lane;
use crate::store::StoreError;

/// One movement of SOL or a token seen in a sampled wallet's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flow {
    pub signature: String,
    pub at: Option<i64>,
    pub direction: Direction,
    pub counterparty: String,
    /// `None` is native SOL.
    pub mint: Option<String>,
    pub amount_raw: u64,
}

/// A bounded look at a wallet's recent activity.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub flows: Vec<Flow>,
    pub trades: Vec<TradeCore>,
    /// Signatures the node listed.
    pub signatures: usize,
    /// The listing was shorter than the limit: this is the wallet's whole history.
    pub complete: bool,
    /// Calls spent: one for the listing and one per transaction read.
    pub credits: u64,
}

impl Sample {
    pub fn senders(&self) -> BTreeSet<&str> {
        self.flows.iter().filter(|f| f.direction == Direction::In).map(|f| f.counterparty.as_str()).collect()
    }

    pub fn outs(&self) -> impl Iterator<Item = &Flow> {
        self.flows.iter().filter(|f| f.direction == Direction::Out)
    }
}

#[derive(Debug)]
pub enum JobError {
    /// The forensics allowance for today is spent, or the node is unreachable.
    Wait(String),
    Failed(String),
}

impl From<RpcError> for JobError {
    fn from(error: RpcError) -> Self {
        match error {
            RpcError::Budget(text) => JobError::Wait(text),
            other => JobError::Failed(other.to_string()),
        }
    }
}

impl From<StoreError> for JobError {
    fn from(error: StoreError) -> Self {
        JobError::Failed(error.to_string())
    }
}

pub enum JobEnd {
    Done,
    /// Run again after this many seconds, with this payload.
    Later(i64, Value),
}

pub type JobResult = Result<JobEnd, JobError>;

pub(crate) const MAX_ATTEMPTS: i32 = 4;

impl App {
    /// Queues forensics work. Quietly does nothing when the same job is already waiting.
    pub async fn enqueue(&self, kind: &str, subject: &str, entity: Option<EntityId>, payload: Value, delay_s: i64) {
        let now = self.now();
        let job = NewJob { kind: kind.to_string(), subject: subject.to_string(), entity_id: entity, payload, run_after: now + delay_s };
        if let Err(error) = self.store.enqueue_job(&job, now).await {
            log::warn!("could not queue {kind} for {subject}: {error}");
        }
    }

    /// Runs the oldest due job. False when nothing was due.
    pub async fn run_next_job(self: &Arc<Self>) -> bool {
        let job = match self.store.claim_job(self.now()).await {
            Ok(Some(job)) => job,
            Ok(None) => return false,
            Err(error) => {
                log::warn!("could not read the job queue: {error}");
                return false;
            }
        };
        let result = match job.kind.as_str() {
            "trace_back" => self.job_trace_back(&job).await,
            "inspect" => self.job_inspect(&job).await,
            "walk" => self.job_walk(&job).await,
            "cobuy" => self.job_cobuy(&job).await,
            "map" => self.job_map(&job).await,
            "review" => self.job_review(&job).await,
            other => Err(JobError::Failed(format!("unknown job kind {other}"))),
        };
        let outcome = match result {
            Ok(JobEnd::Done) => self.store.finish_job(job.id, true).await,
            Ok(JobEnd::Later(seconds, payload)) if job.attempts < MAX_ATTEMPTS => self.store.retry_job(job.id, self.now() + seconds, &payload).await,
            Ok(JobEnd::Later(..)) => self.store.finish_job(job.id, true).await,
            // Out of allowance: the job keeps its place and waits an hour.
            Err(JobError::Wait(why)) => {
                log::info!("{} for {} waits: {why}", job.kind, job.subject);
                self.store.retry_job(job.id, self.now() + 3_600, &job.payload).await
            }
            Err(JobError::Failed(why)) if job.attempts < MAX_ATTEMPTS => {
                log::warn!("{} for {} failed (attempt {}): {why}", job.kind, job.subject, job.attempts);
                self.store.retry_job(job.id, self.now() + 300 * job.attempts as i64, &job.payload).await
            }
            Err(JobError::Failed(why)) => {
                log::warn!("{} for {} gave up: {why}", job.kind, job.subject);
                self.store.finish_job(job.id, false).await
            }
        };
        if let Err(error) = outcome {
            log::warn!("could not record the end of job {}: {error}", job.id);
        }
        true
    }

    /// Runs due jobs until none are left or `max` have run. Returns how many ran.
    pub async fn drain_jobs(self: &Arc<Self>, max: usize) -> usize {
        let mut ran = 0;
        while ran < max && self.run_next_job().await {
            ran += 1;
        }
        ran
    }

    /// Reads up to `tx_limit` of a wallet's newest transactions and lists
    /// what moved. Transactions older than `since` are not read.
    pub async fn sample_wallet(&self, address: &str, sig_limit: usize, tx_limit: usize, since: Option<i64>, lane: Lane) -> Result<Sample, RpcError> {
        let rows = self.rpc.get_signatures(address, None, sig_limit, lane).await?;
        let mut sample = Sample { signatures: rows.len(), complete: rows.len() < sig_limit, credits: 1, ..Sample::default() };
        for row in rows.iter().filter(|r| !r.failed).take(tx_limit) {
            if let (Some(since), Some(at)) = (since, row.block_time)
                && at < since
            {
                break;
            }
            self.read_into(&mut sample, address, row, lane).await?;
        }
        Ok(sample)
    }

    async fn read_into(&self, sample: &mut Sample, address: &str, row: &SignatureInfo, lane: Lane) -> Result<(), RpcError> {
        sample.credits += 1;
        let Some(raw) = self.rpc.get_transaction(&row.signature, lane).await? else { return Ok(()) };
        let Some(tx) = ParsedTx::from_value(&raw) else { return Ok(()) };
        match classify(&tx, address) {
            Classified::Trade(trade) => sample.trades.push(trade),
            Classified::Transfers(found) => {
                for core in found {
                    if self.labels.is_class(&core.counterparty, LabelClass::Tip) {
                        continue;
                    }
                    sample.flows.push(Flow {
                        signature: row.signature.clone(),
                        at: tx.block_time.or(row.block_time),
                        direction: core.direction,
                        counterparty: core.counterparty,
                        mint: core.mint,
                        amount_raw: core.amount_raw,
                    });
                }
            }
            Classified::Other => {}
        }
        Ok(())
    }

    /// The exchange a labelled hot wallet belongs to: the first word of its
    /// label, so "Binance 2" and "Binance 3" are one exchange.
    pub fn exchange_of(&self, address: &str) -> Option<String> {
        self.labels.get(address).filter(|l| l.class == LabelClass::CexHot).map(|l| l.name.split_whitespace().next().unwrap_or("exchange").to_string())
    }

    /// The deposit test, on a sample of an address's history: its outflows
    /// are swept to one exchange's hot wallets. Returns the exchange.
    pub fn swept_to_exchange(&self, sample: &Sample) -> Option<String> {
        let outs: Vec<&Flow> = sample.outs().collect();
        let to_exchange: Vec<String> = outs.iter().filter_map(|f| self.exchange_of(&f.counterparty)).collect();
        if to_exchange.is_empty() || to_exchange.len() * 10 < outs.len() * 9 {
            return None;
        }
        let mut names = to_exchange.clone();
        names.sort();
        names.dedup();
        names.into_iter().max_by_key(|name| to_exchange.iter().filter(|n| *n == name).count())
    }

    /// Marks an address that pays or collects from far too many wallets as a
    /// service, so it is never followed or linked again.
    pub async fn learn_hub(&self, address: &str, counterparties: usize) {
        let label = Label { address: address.to_string(), class: LabelClass::Hub, name: format!("learned hub ({counterparties}+ counterparties)"), source: "learned".to_string() };
        if self.store.put_label(&label).await.is_ok() {
            self.labels.set(label);
            log::info!("{address} learned as a hub ({counterparties} counterparties in one sample)");
        }
    }

    pub(crate) fn payload_str<'a>(job: &'a Job, key: &str) -> &'a str {
        job.payload.get(key).and_then(Value::as_str).unwrap_or("")
    }

    pub(crate) fn payload_u64(job: &Job, key: &str) -> u64 {
        job.payload.get(key).and_then(Value::as_u64).unwrap_or(0)
    }

    pub(crate) async fn bump_proofs(&self) {
        let proofs = self.proof_count().await + 1;
        let _ = self.store.set_state("proofs", &proofs.to_string()).await;
    }
}
