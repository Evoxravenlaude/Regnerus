//! Co-buy windows. When an entity buys, the transactions around its buy
//! are read for other wallets buying the same token in the same moment.
//! One co-buy means little; the same wallet beside the same entity on
//! token after token is a side wallet or a copier.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::App;
use crate::chain::solana::query::signatures_before;
use crate::chain::solana::tx::{ParsedTx, classify_trade};
use crate::identity::Unique;
use crate::identity::behaviour::{fingerprint_points, rotated};
use crate::identity::score::kind;
use crate::model::*;
use crate::obs::budget::Lane;
use crate::store::StoreResult;
use crate::trail::{JobEnd, JobResult};

const WINDOW_S: i64 = 60;
const TIGHT_SLOTS: u64 = 3;
const LISTING: usize = 40;
/// Co-buying beside this many different entities is a sniper, not a side wallet.
const SNIPER_ENTITIES: usize = 3;

impl App {
    /// Reads the transactions around one entity buy and records who else bought.
    pub(crate) async fn job_cobuy(self: &Arc<Self>, job: &Job) -> JobResult {
        let Some(entity) = job.entity_id else { return Ok(JobEnd::Done) };
        let signature = job.subject.as_str();
        let mint = App::payload_str(job, "mint").to_string();
        let slot = App::payload_u64(job, "slot");
        let at = job.payload.get("at").and_then(Value::as_i64).unwrap_or(0);
        if self.settings.cobuy_txs == 0 || mint.is_empty() {
            return Ok(JobEnd::Done);
        }
        // Everything that mentions the token just before and just after the buy.
        let mut rows = self.rpc.get_signatures(&mint, Some(signature), LISTING, Lane::Forensics).await?;
        rows.reverse(); // closest to the buy first
        rows.extend(signatures_before(self.rpc.as_ref(), &mint, Some(signature), LISTING, Lane::Forensics).await?);
        let near: Vec<_> = rows
            .into_iter()
            .filter(|row| !row.failed)
            .filter(|row| match row.block_time {
                Some(when) if at > 0 => (when - at).abs() <= WINDOW_S,
                _ => row.slot.abs_diff(slot) <= 150,
            })
            .collect();
        let key = format!("{mint}:{entity}");
        if near.len() + 1 >= self.settings.cobuy_thin {
            // A crowd in the same moment: co-buying here says nothing, and
            // this buy does not count toward consensus.
            self.store.put_doc("crowd", &key, &json!({ "signature": signature, "others": near.len(), "at": at }), self.now()).await?;
            return Ok(JobEnd::Done);
        }

        let own: BTreeSet<String> = self.store.wallets_of(entity).await?.into_iter().filter(|w| w.state == WalletState::Linked).map(|w| w.address).collect();
        for row in near.iter().take(self.settings.cobuy_txs) {
            let Some(raw) = self.rpc.get_transaction(&row.signature, Lane::Forensics).await? else { continue };
            let Some(tx) = ParsedTx::from_value(&raw) else { continue };
            // The buyer signed it; try each signer until one reads as a buy of this token.
            let signers: Vec<String> = tx.transaction.message.account_keys.iter().filter(|k| k.signer()).map(|k| k.pubkey().to_string()).take(3).collect();
            let Some(buyer) = signers.into_iter().find(|s| classify_trade(&tx, s).is_some_and(|t| t.side == Side::Buy && t.mint == mint)) else { continue };
            if own.contains(&buyer) || self.labels.get(&buyer).is_some() {
                continue;
            }
            let slots = tx.slot.abs_diff(slot);
            self.note_cobuy(&buyer, entity, &mint, slots, &row.signature).await?;
        }
        Ok(JobEnd::Done)
    }

    /// One wallet bought one token beside one entity.
    pub(crate) async fn note_cobuy(self: &Arc<Self>, buyer: &str, entity: EntityId, mint: &str, slots: u64, signature: &str) -> StoreResult<()> {
        if self.store.doc("sniper", buyer).await?.is_some() {
            return Ok(());
        }
        self.store.put_doc("cobuy", &format!("{buyer}:{entity}:{mint}"), &json!({ "slots": slots, "signature": signature }), self.now()).await?;
        let seen = self.store.docs("cobuy", &format!("{buyer}:"), 500).await?;
        let entities: BTreeSet<&str> = seen.iter().filter_map(|(key, _)| key.split(':').nth(1)).collect();
        if entities.len() >= SNIPER_ENTITIES {
            // Buys beside everyone: its co-buys are noise and are withdrawn.
            self.store.put_doc("sniper", buyer, &json!({ "entities": entities.len() }), self.now()).await?;
            let rows = self.store.evidence_of_wallet(buyer).await?;
            let mut touched = BTreeSet::new();
            for row in rows.iter().filter(|r| !r.voided && (r.kind == kind::COBUY_SLOTS || r.kind == kind::COBUY_MINUTE)) {
                self.store.void_evidence_row(row.id).await?;
                touched.insert(row.entity_id);
            }
            for entity in touched {
                self.rejudge(buyer, entity).await?;
            }
            return Ok(());
        }
        let kind_name = if slots <= TIGHT_SLOTS { kind::COBUY_SLOTS } else { kind::COBUY_MINUTE };
        self.add_evidence(buyer, entity, kind_name, None, vec![signature.to_string()], json!({ "mint": mint, "slots": slots }), Unique::Key(mint.to_string())).await?;
        let beside = seen.iter().filter(|(key, _)| key.split(':').nth(1) == Some(entity.to_string().as_str())).count();
        if beside >= 2 && self.store.doc("funder", buyer).await?.is_none() {
            // Twice beside the same entity: worth knowing who funded it.
            self.enqueue("trace_back", buyer, Some(entity), json!({ "hops": 1 }), 0).await;
        }
        Ok(())
    }

    /// `note_cobuy`, for tests that need a co-buy without a chain to read it from.
    #[doc(hidden)]
    pub async fn note_cobuy_for_test(self: &Arc<Self>, buyer: &str, entity: EntityId, mint: &str, slots: u64) {
        self.note_cobuy(buyer, entity, mint, slots, "").await.expect("store");
    }

    /// A watched wallet that is not linked traded: refresh what its
    /// behaviour says about belonging to the entity.
    pub(crate) async fn on_candidate_trade(self: &Arc<Self>, wallet: &Wallet, trade: &Trade) -> StoreResult<()> {
        let Some(entity) = wallet.entity_id else { return Ok(()) };
        let at = trade.block_time.unwrap_or(trade.seen_at);
        let own: Vec<Wallet> = self.store.wallets_of(entity).await?.into_iter().filter(|w| w.state == WalletState::Linked).collect();
        let theirs = self.store.trades_of_entity(entity, 40).await?;
        let mine = self.store.trades_of_wallet(&wallet.address, 40).await?;

        if trade.side == Side::Buy && self.store.doc("crowd", &format!("{}:{entity}", trade.mint)).await?.is_none() {
            // Bought beside the entity, as seen in the bot's own records.
            let beside = theirs.iter().find(|t| t.side == Side::Buy && t.mint == trade.mint && (t.block_time.unwrap_or(t.seen_at) - at).abs() <= WINDOW_S);
            if let Some(leader) = beside {
                self.note_cobuy(&wallet.address, entity, &trade.mint, leader.slot.abs_diff(trade.slot), &trade.signature).await?;
            }
        }
        let prints = |trades: &[Trade]| trades.iter().map(|t| t.fingerprint.clone()).collect::<Vec<_>>();
        let points = fingerprint_points(&prints(&mine), &prints(&theirs));
        if points > 0 {
            self.add_evidence(&wallet.address, entity, kind::FINGERPRINT, Some(points), Vec::new(), json!({ "trades": mine.len() }), Unique::Replace).await?;
        }
        let first = mine.iter().map(|t| t.block_time.unwrap_or(t.seen_at)).min().unwrap_or(at);
        let mut last_of: Vec<i64> = Vec::new();
        for linked in &own {
            if let Some(last) = self.store.trades_of_wallet(&linked.address, 1).await?.first() {
                last_of.push(last.block_time.unwrap_or(last.seen_at));
            }
        }
        if rotated(first, &last_of, self.now()) {
            self.add_evidence(&wallet.address, entity, kind::ROTATION, None, Vec::new(), json!({ "first_trade": first }), Unique::Replace).await?;
        }
        Ok(())
    }
}
