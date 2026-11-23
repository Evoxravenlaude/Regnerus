//! Exit watch: where a watched wallet's money goes. Three things can be at
//! the other end: an exchange deposit address (proof for everyone who pays
//! into it), a fresh wallet (a successor), or a shared sink.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{JobEnd, JobResult};
use crate::app::App;
use crate::chain::solana::tx::{ParsedTx, USDC_MINT, USDT_MINT, WSOL_MINT, extract_transfers};
use crate::identity::Unique;
use crate::identity::score::kind;
use crate::model::*;
use crate::obs::budget::Lane;
use crate::store::StoreResult;

/// A stablecoin transfer below this is not followed ($50).
const MIN_STABLE_RAW: u64 = 50_000_000;
/// More counterparties than this in one sample of 25 transactions is a service, not a person.
const HUB_COUNTERPARTIES: usize = 20;
const SAMPLE_SIGNATURES: usize = 40;
const SAMPLE_TXS: usize = 25;

fn is_quote_mint(mint: &str) -> bool {
    matches!(mint, WSOL_MINT | USDC_MINT | USDT_MINT)
}

impl App {
    /// Called for every new transfer of a watched wallet. Cheap checks run
    /// here; anything that needs the chain is queued.
    pub(crate) async fn on_transfer(self: &Arc<Self>, wallet: &Wallet, transfer: &Transfer) -> StoreResult<()> {
        let Some(entity) = wallet.entity_id else { return Ok(()) };
        if transfer.direction != Direction::Out || !matches!(wallet.state, WalletState::Linked | WalletState::Candidate) {
            return Ok(());
        }
        let dest = transfer.counterparty.as_str();
        let at = transfer.block_time.unwrap_or_else(|| self.now());
        let linked = wallet.state == WalletState::Linked;

        // A known deposit address: the cheapest and strongest case.
        if let Some(deposit) = self.store.deposit(dest).await? {
            if deposit.status != DepositStatus::Qualified {
                return Ok(());
            }
            if deposit.entity_id == entity && linked {
                if transfer.mint.is_none() {
                    let _ = self.record_cex_deposit(entity, &deposit.exchange, &wallet.address, transfer.amount_raw, at, &transfer.signature).await;
                }
            } else {
                self.deposit_proof(&wallet.address, &deposit, &transfer.signature).await?;
            }
            return Ok(());
        }
        let big_enough = match transfer.mint.as_deref() {
            None => transfer.amount_raw >= self.settings.exit_min_lamports,
            Some(USDC_MINT | USDT_MINT) => transfer.amount_raw >= MIN_STABLE_RAW,
            Some(_) => true,
        };
        if !big_enough || self.labels.get(dest).is_some() || self.store.wallet(dest).await?.is_some_and(|w| w.entity_id == Some(entity)) {
            return Ok(());
        }
        let payload = json!({ "from": wallet.address, "linked": linked, "mint": transfer.mint, "amount": transfer.amount_raw, "signature": transfer.signature, "at": at });
        // A moment's wait, so a deposit has time to be swept on.
        self.enqueue("inspect", dest, Some(entity), payload, 90).await;
        Ok(())
    }

    /// A wallet paid into an entity's qualified deposit address: proof.
    pub(crate) async fn deposit_proof(self: &Arc<Self>, sender: &str, deposit: &DepositAddr, signature: &str) -> StoreResult<()> {
        if self.labels.get(sender).is_some() {
            return Ok(());
        }
        let details = json!({ "deposit": deposit.address, "exchange": deposit.exchange });
        let change = self.add_evidence(sender, deposit.entity_id, kind::DEPOSIT_PROOF, Some(100), vec![signature.to_string()], details, Unique::Key(deposit.address.clone())).await?;
        if change.is_some() {
            self.bump_proofs().await;
        }
        Ok(())
    }

    /// A transaction touched a deposit address the stream watches: whoever
    /// paid in belongs to the address's entity.
    pub(crate) async fn on_tripwire(self: &Arc<Self>, deposit: &DepositAddr, tx: &ParsedTx, signature: &str) -> StoreResult<usize> {
        let mut proofs = 0;
        for transfer in extract_transfers(tx, &deposit.address) {
            if transfer.direction != Direction::In {
                continue;
            }
            let known = self.store.wallet(&transfer.counterparty).await?.is_some_and(|w| w.state == WalletState::Linked && w.entity_id == Some(deposit.entity_id));
            if !known {
                self.deposit_proof(&transfer.counterparty, deposit, signature).await?;
                proofs += 1;
            }
        }
        Ok(proofs)
    }

    /// Looks at the far end of an outgoing transfer and decides what it is.
    pub(crate) async fn job_inspect(self: &Arc<Self>, job: &Job) -> JobResult {
        let dest = job.subject.as_str();
        let Some(entity) = job.entity_id else { return Ok(JobEnd::Done) };
        let from = App::payload_str(job, "from").to_string();
        let linked = job.payload.get("linked").and_then(Value::as_bool).unwrap_or(false);
        let mint = job.payload.get("mint").and_then(Value::as_str).map(str::to_string);
        let amount = App::payload_u64(job, "amount");
        let signature = App::payload_str(job, "signature").to_string();
        let at = job.payload.get("at").and_then(Value::as_i64).unwrap_or_else(|| self.now());
        if self.labels.get(dest).is_some() || self.store.deposit(dest).await?.is_some() {
            return Ok(JobEnd::Done);
        }

        let sample = self.sample_wallet(dest, SAMPLE_SIGNATURES, SAMPLE_TXS, None, Lane::Forensics).await?;
        let senders: BTreeSet<String> = sample.senders().into_iter().map(str::to_string).collect();
        let counterparties: BTreeSet<&str> = sample.flows.iter().map(|f| f.counterparty.as_str()).collect();
        if counterparties.len() > HUB_COUNTERPARTIES {
            self.learn_hub(dest, counterparties.len()).await;
            return Ok(JobEnd::Done);
        }

        if let Some(exchange) = self.swept_to_exchange(&sample) {
            let personal = senders.len() <= self.settings.deposit_max_senders;
            if !linked {
                // A candidate with its own deposit address at an exchange
                // where the entity has another: weak evidence against.
                let theirs = self.store.deposits_of(entity).await?.into_iter().any(|d| d.exchange == exchange && d.status == DepositStatus::Qualified && d.address != dest);
                if personal && theirs {
                    self.add_evidence(&from, entity, kind::OTHER_DEPOSIT, None, vec![signature], json!({ "deposit": dest, "exchange": exchange }), Unique::Key(dest.to_string())).await?;
                }
                return Ok(JobEnd::Done);
            }
            let status = if personal { DepositStatus::Qualified } else { DepositStatus::Rejected };
            let deposit = DepositAddr { address: dest.to_string(), entity_id: entity, exchange: exchange.clone(), status, senders: senders.len() as i32, first_seen: self.now() };
            self.store.put_deposit(&deposit).await?;
            log::info!("{dest} is a {exchange} deposit address of E#{entity}: {} senders, {status}", senders.len());
            if personal {
                // From now on the stream watches it, and everyone who already paid in is settled.
                self.refresh.notify_one();
                for sender in &senders {
                    let own = self.store.wallet(sender).await?.is_some_and(|w| w.state == WalletState::Linked && w.entity_id == Some(entity));
                    if !own {
                        let paid = sample.flows.iter().find(|f| f.direction == Direction::In && &f.counterparty == sender).map(|f| f.signature.clone()).unwrap_or_default();
                        self.deposit_proof(sender, &deposit, &paid).await?;
                    }
                }
                if mint.is_none() {
                    self.record_cex_deposit(entity, &exchange, &from, amount, at, &signature).await?;
                }
            }
            return Ok(JobEnd::Done);
        }
        if sample.outs().next().is_none() && sample.trades.is_empty() && sample.signatures <= 3 && job.attempts < super::MAX_ATTEMPTS {
            // Nothing has left it yet: a deposit not swept, or a wallet not
            // used. Look again later. An exchange sweeps within minutes, so
            // an address still idle on the last look is taken for a wallet.
            return Ok(JobEnd::Later(900, job.payload.clone()));
        }
        if !linked {
            return Ok(JobEnd::Done);
        }

        match mint.as_deref().filter(|m| !is_quote_mint(m)) {
            Some(token) => {
                // Hand-off: the entity bought the token, sent it here, and this wallet sold it.
                let bought = self.store.trades_of_mint(token, 0, 500).await?.iter().any(|t| t.side == Side::Buy && t.entity_id == Some(entity));
                let sold = sample.trades.iter().any(|t| t.side == Side::Sell && t.mint == token);
                if bought && sold {
                    self.add_evidence(dest, entity, kind::TOKEN_HANDOFF, None, vec![signature.clone()], json!({ "mint": token, "from": from }), Unique::Key(signature)).await?;
                } else if bought {
                    return Ok(JobEnd::Later(1_800, job.payload.clone()));
                }
            }
            None if mint.is_none() => {
                let first_in = sample.flows.iter().rev().find(|f| f.direction == Direction::In && f.mint.is_none());
                let fresh = sample.complete && first_in.is_some_and(|f| f.counterparty == from);
                if fresh {
                    self.add_evidence(dest, entity, kind::DIRECT_FUNDING, None, vec![signature.clone()], json!({ "funder": from, "lamports": amount }), Unique::Key(signature)).await?;
                } else if senders.len() <= self.settings.deposit_max_senders {
                    // An established plain wallet that few others pay: a shared exit sink.
                    self.store.put_doc("sink", dest, &json!({ "entity": entity, "from": from }), self.now()).await?;
                    for sender in senders.iter().filter(|s| **s != from) {
                        if self.labels.get(sender).is_some() || self.store.wallet(sender).await?.is_some_and(|w| w.entity_id == Some(entity) && w.state == WalletState::Linked) {
                            continue;
                        }
                        let paid = sample.flows.iter().find(|f| f.direction == Direction::In && &f.counterparty == sender).map(|f| f.signature.clone()).unwrap_or_default();
                        self.add_evidence(sender, entity, kind::SHARED_SINK, None, vec![paid], json!({ "sink": dest }), Unique::Key(dest.to_string())).await?;
                    }
                }
            }
            // A stablecoin to a plain wallet: a payment, not a trail.
            None => {}
        }
        Ok(JobEnd::Done)
    }
}
