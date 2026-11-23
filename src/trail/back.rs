//! Trace back: who first funded a wallet. And the CEX hop match, which
//! pairs an entity's exchange deposit with a fresh wallet's withdrawal.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{JobEnd, JobError, JobResult};
use crate::app::App;
use crate::chain::solana::query::signatures_before;
use crate::chain::solana::tx::{Classified, ParsedTx, classify};
use crate::identity::Unique;
use crate::identity::score::kind;
use crate::model::*;
use crate::obs::budget::Lane;

/// A wallet with more history than this is not fresh: its first funding is
/// too far back to be worth the credits.
const MAX_PAGES: usize = 3;
const PAGE: usize = 1_000;
/// An exchange keeps a withdrawal fee; a match may fall short of the deposit by this much...
const FEE_ALLOWANCE_LAMPORTS: u64 = 20_000_000;
/// ...plus this share of the deposit.
const LOOSE_SHARE: f64 = 0.03;
const TIGHT_SHARE: f64 = 0.005;
const TIGHT_SECONDS: i64 = 3_600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Funder {
    pub funder: String,
    pub lamports: u64,
    pub at: Option<i64>,
    pub signature: String,
}

/// Whether an amount of SOL looks typed by a person (1, 2.5, 10): such
/// amounts match by chance far more often.
pub fn is_round(lamports: u64) -> bool {
    lamports.is_multiple_of(100_000_000)
}

/// How well a withdrawal fits a deposit: `None`, loose, or tight.
pub fn hop_fit(deposit_lamports: u64, deposit_at: i64, withdrawal_lamports: u64, withdrawal_at: i64, window_s: i64) -> Option<bool> {
    let delay = withdrawal_at - deposit_at;
    if delay <= 0 || delay > window_s || withdrawal_lamports > deposit_lamports {
        return None;
    }
    let short = deposit_lamports - withdrawal_lamports;
    if short > FEE_ALLOWANCE_LAMPORTS + (deposit_lamports as f64 * LOOSE_SHARE) as u64 {
        return None;
    }
    let tight = short <= FEE_ALLOWANCE_LAMPORTS + (deposit_lamports as f64 * TIGHT_SHARE) as u64 && delay <= TIGHT_SECONDS && !is_round(deposit_lamports);
    Some(tight)
}

impl App {
    /// The first inbound SOL transfer of a wallet. `None` when the wallet
    /// has too long a history to be fresh, or was not funded by a transfer.
    pub async fn first_funder(&self, wallet: &str, lane: Lane) -> Result<Option<Funder>, JobError> {
        let mut oldest: Vec<crate::chain::solana::rpc::SignatureInfo> = Vec::new();
        let mut before: Option<String> = None;
        for page in 0..MAX_PAGES {
            let rows = signatures_before(self.rpc.as_ref(), wallet, before.as_deref(), PAGE, lane).await?;
            let full = rows.len() >= PAGE;
            if let Some(last) = rows.last() {
                before = Some(last.signature.clone());
                oldest = rows;
            }
            if !full {
                break;
            }
            if page + 1 == MAX_PAGES {
                return Ok(None);
            }
        }
        // The funding transfer is the first transaction, or close to it.
        for row in oldest.iter().rev().filter(|r| !r.failed).take(3) {
            let Some(raw) = self.rpc.get_transaction(&row.signature, lane).await? else { continue };
            let Some(tx) = ParsedTx::from_value(&raw) else { continue };
            if let Classified::Transfers(found) = classify(&tx, wallet)
                && let Some(funding) = found.into_iter().find(|t| t.direction == Direction::In && t.mint.is_none())
            {
                return Ok(Some(Funder { funder: funding.counterparty, lamports: funding.amount_raw, at: tx.block_time.or(row.block_time), signature: row.signature.clone() }));
            }
        }
        Ok(None)
    }

    /// Finds who funded a wallet and acts on the answer: evidence when a
    /// linked wallet did, a hop match when an exchange did, one more hop
    /// upward when a plain wallet did.
    pub(crate) async fn job_trace_back(self: &Arc<Self>, job: &Job) -> JobResult {
        let wallet = job.subject.as_str();
        let hops = App::payload_u64(job, "hops");
        if self.labels.get(wallet).is_some() {
            return Ok(JobEnd::Done);
        }
        let cached = self.store.doc("funder", wallet).await?;
        let funder = match cached {
            Some(doc) => doc.get("funder").and_then(Value::as_str).map(|f| Funder {
                funder: f.to_string(),
                lamports: doc.get("lamports").and_then(Value::as_u64).unwrap_or(0),
                at: doc.get("at").and_then(Value::as_i64),
                signature: doc.get("signature").and_then(Value::as_str).unwrap_or("").to_string(),
            }),
            None => {
                let found = self.first_funder(wallet, Lane::Forensics).await?;
                let doc = match &found {
                    Some(f) => json!({ "funder": f.funder, "lamports": f.lamports, "at": f.at, "signature": f.signature }),
                    None => json!({ "funder": null }),
                };
                self.store.put_doc("funder", wallet, &doc, self.now()).await?;
                found
            }
        };
        let Some(funder) = funder else { return Ok(JobEnd::Done) };

        if let Some(exchange) = self.exchange_of(&funder.funder) {
            // An exchange withdrawal: the trail goes cold here, unless it
            // pairs with a deposit an entity made just before.
            let key = format!("{exchange}:{wallet}");
            self.store.put_doc("cexfund", &key, &json!({ "wallet": wallet, "exchange": exchange, "lamports": funder.lamports, "at": funder.at, "signature": funder.signature }), self.now()).await?;
            self.hop_match(&exchange).await?;
            // For a wallet that is not yet the entity's: does its funding look like theirs?
            if let Some(entity) = job.entity_id
                && !self.store.wallet(wallet).await?.is_some_and(|w| w.state == WalletState::Linked)
            {
                self.check_habit(wallet, entity, &exchange, funder.lamports).await?;
            }
            return Ok(JobEnd::Done);
        }
        if self.labels.get(&funder.funder).is_some() {
            return Ok(JobEnd::Done);
        }
        match self.store.wallet(&funder.funder).await? {
            Some(source) if source.state == WalletState::Linked => {
                let Some(entity) = source.entity_id else { return Ok(JobEnd::Done) };
                let own = self.store.wallet(wallet).await?.is_some_and(|w| w.state == WalletState::Linked && w.entity_id == Some(entity));
                if !own {
                    let details = json!({ "funder": funder.funder, "lamports": funder.lamports });
                    self.add_evidence(wallet, entity, kind::DIRECT_FUNDING, None, vec![funder.signature.clone()], details, Unique::Key(funder.signature.clone())).await?;
                }
            }
            known => {
                // The other direction: a plain wallet that gave a linked
                // wallet its first SOL is a parent in sight. Evidence, not
                // a link: an admin or more evidence settles it.
                let child = self.store.wallet(wallet).await?.filter(|w| w.state == WalletState::Linked);
                if let Some(entity) = child.and_then(|w| w.entity_id)
                    && known.as_ref().is_none_or(|w| w.entity_id != Some(entity) || w.state == WalletState::Seen)
                {
                    let details = json!({ "funded": wallet, "lamports": funder.lamports });
                    self.add_evidence(&funder.funder, entity, kind::FUNDED_LINKED, None, vec![funder.signature.clone()], details, Unique::Key(funder.signature.clone())).await?;
                } else if hops > 0 {
                    self.enqueue("trace_back", &funder.funder, job.entity_id, json!({ "hops": hops - 1, "below": wallet }), 0).await;
                }
            }
        }
        Ok(JobEnd::Done)
    }

    /// Habit: funded from the same exchange, in the same amount band, as
    /// the entity's own wallets were.
    async fn check_habit(self: &Arc<Self>, wallet: &str, entity: EntityId, exchange: &str, lamports: u64) -> Result<(), JobError> {
        let linked: Vec<Wallet> = self.store.wallets_of(entity).await?.into_iter().filter(|w| w.state == WalletState::Linked && w.address != wallet).collect();
        for own in linked {
            let Some(doc) = self.store.doc("cexfund", &format!("{exchange}:{}", own.address)).await? else { continue };
            let theirs = doc.get("lamports").and_then(Value::as_u64).unwrap_or(0);
            if theirs > 0 && lamports >= theirs / 2 && lamports <= theirs.saturating_mul(2) {
                self.add_evidence(wallet, entity, kind::HABIT, None, Vec::new(), json!({ "exchange": exchange, "lamports": lamports, "like": own.address }), Unique::Replace).await?;
                break;
            }
        }
        Ok(())
    }

    /// Records that an entity's wallet paid `lamports` into an exchange.
    pub(crate) async fn record_cex_deposit(self: &Arc<Self>, entity: EntityId, exchange: &str, wallet: &str, lamports: u64, at: i64, signature: &str) -> Result<(), JobError> {
        let key = format!("{exchange}:{entity}:{signature}");
        self.store.put_doc("cexflow", &key, &json!({ "entity": entity, "exchange": exchange, "wallet": wallet, "lamports": lamports, "at": at, "signature": signature }), self.now()).await?;
        self.hop_match(exchange).await
    }

    /// Pairs every recorded deposit at an exchange with the withdrawals to
    /// fresh wallets the bot has traced. One clean fit is strong, a few is
    /// weak, many means nothing.
    pub async fn hop_match(self: &Arc<Self>, exchange: &str) -> Result<(), JobError> {
        let prefix = format!("{exchange}:");
        let flows = self.store.docs("cexflow", &prefix, 500).await?;
        let funds = self.store.docs("cexfund", &prefix, 2_000).await?;
        for (_, flow) in flows {
            let (Some(entity), Some(deposited), Some(at), Some(signature)) =
                (flow.get("entity").and_then(Value::as_i64), flow.get("lamports").and_then(Value::as_u64), flow.get("at").and_then(Value::as_i64), flow.get("signature").and_then(Value::as_str))
            else {
                continue;
            };
            let mut fits: Vec<(String, u64, i64, bool)> = Vec::new();
            for (_, fund) in &funds {
                let (Some(wallet), Some(withdrawn), Some(when)) = (fund.get("wallet").and_then(Value::as_str), fund.get("lamports").and_then(Value::as_u64), fund.get("at").and_then(Value::as_i64))
                else {
                    continue;
                };
                if let Some(tight) = hop_fit(deposited, at, withdrawn, when, self.settings.hop_window_s) {
                    fits.push((wallet.to_string(), withdrawn, when, tight));
                }
            }
            for (wallet, withdrawn, when, tight) in &fits {
                if self.store.wallet(wallet).await?.is_some_and(|w| w.state == WalletState::Linked && w.entity_id == Some(entity)) {
                    continue;
                }
                let kind_name = match (fits.len(), tight) {
                    (1, true) => kind::CEX_HOP_TIGHT,
                    (1..=3, _) => kind::CEX_HOP_LOOSE,
                    _ => kind::CEX_HOP_WEAK,
                };
                // A fit is graded once: when more fits turn up, the old grade is replaced.
                let graded: Vec<Evidence> = self
                    .store
                    .evidence_of_wallet(wallet)
                    .await?
                    .into_iter()
                    .filter(|e| e.entity_id == entity && !e.voided && e.kind.starts_with("cex_hop") && e.details.get("key").and_then(Value::as_str) == Some(signature))
                    .collect();
                if graded.iter().any(|e| e.kind == kind_name) {
                    continue;
                }
                for row in graded {
                    self.store.void_evidence_row(row.id).await?;
                }
                let details = json!({ "exchange": exchange, "deposited": deposited, "withdrawn": withdrawn, "minutes": (when - at) / 60, "fits": fits.len() });
                self.add_evidence(wallet, entity, kind_name, None, vec![signature.to_string()], details, Unique::Key(signature.to_string())).await?;
            }
        }
        Ok(())
    }
}
