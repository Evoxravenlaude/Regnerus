//! The bubble map: one token's largest holders, grouped by what ties them
//! together. A cluster is a question, not a verdict.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::App;
use crate::chain::solana::query::{accounts, largest_accounts, token_supply};
use crate::chain::solana::tx::SYSTEM_PROGRAM;
use crate::model::*;
use crate::obs::budget::Lane;
use crate::trail::JobError;

/// Holders whose funder is looked up when it is not already known.
const TRACE_HOLDERS: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub owner: String,
    pub amount: u64,
    /// A pool, a bonding curve or a labelled service: held, but not by a trader.
    pub service: Option<String>,
    pub entity: Option<EntityId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cluster {
    pub members: Vec<String>,
    pub amount: u64,
    /// Why the members belong together, one line per tie.
    pub ties: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MapReport {
    pub mint: String,
    pub supply: u64,
    pub holders: Vec<Holder>,
    /// Groups of two or more, largest first.
    pub clusters: Vec<Cluster>,
    pub credits: u64,
}

impl MapReport {
    /// The largest cluster's share of supply, in hundredths of a percent.
    pub fn largest_cluster_bps(&self) -> u64 {
        match (self.clusters.first(), self.supply) {
            (Some(cluster), supply) if supply > 0 => (cluster.amount as u128 * 10_000 / supply as u128) as u64,
            _ => 0,
        }
    }
}

/// Groups holders joined by ties. `ties` are `(holder, holder, why)`.
pub fn cluster(holders: &[Holder], ties: &[(String, String, String)]) -> Vec<Cluster> {
    let index: BTreeMap<&str, usize> = holders.iter().enumerate().map(|(i, h)| (h.owner.as_str(), i)).collect();
    let mut parent: Vec<usize> = (0..holders.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for (a, b, _) in ties {
        if let (Some(&a), Some(&b)) = (index.get(a.as_str()), index.get(b.as_str())) {
            let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
            parent[ra.max(rb)] = ra.min(rb);
        }
    }
    let mut groups: BTreeMap<usize, Cluster> = BTreeMap::new();
    for (i, holder) in holders.iter().enumerate() {
        let group = groups.entry(root(&mut parent, i)).or_insert_with(|| Cluster { members: Vec::new(), amount: 0, ties: Vec::new() });
        group.members.push(holder.owner.clone());
        group.amount += holder.amount;
    }
    for (a, b, why) in ties {
        if let Some(&i) = index.get(a.as_str())
            && index.contains_key(b.as_str())
            && let Some(group) = groups.get_mut(&root(&mut parent, i))
            && !group.ties.contains(why)
        {
            group.ties.push(why.clone());
        }
    }
    let mut out: Vec<Cluster> = groups.into_values().filter(|c| c.members.len() > 1).collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.amount));
    out
}

impl App {
    /// `/map`, run from the queue: builds the map and sends it to whoever asked.
    pub(crate) async fn job_map(self: &Arc<Self>, job: &Job) -> crate::trail::JobResult {
        let report = self.bubble_map(&job.subject, Lane::Forensics).await?;
        if let Some(chat) = job.payload.get("chat").and_then(Value::as_i64) {
            let card =
                if report.holders.is_empty() { crate::bot::telegram::Card::text("The node listed no holders for that address. Is it a token mint?") } else { crate::bot::cards::map_card(&report) };
            crate::bot::telegram::send(self.tg.as_ref(), chat, &card).await;
        }
        Ok(crate::trail::JobEnd::Done)
    }

    /// Builds the map of a token's largest holders from what the bot
    /// already knows, plus a bounded number of funder lookups.
    pub async fn bubble_map(self: &Arc<Self>, mint: &str, lane: Lane) -> Result<MapReport, JobError> {
        let rpc = self.rpc.as_ref();
        let supply = token_supply(rpc, mint, lane).await?;
        let largest = largest_accounts(rpc, mint, lane).await?;
        let mut credits = 2;
        let token_accounts: Vec<String> = largest.iter().map(|(a, _)| a.clone()).collect();
        let infos = accounts(rpc, &token_accounts, lane).await?;
        credits += 1;

        // Token accounts to their owners, adding up a holder with several accounts.
        let mut by_owner: BTreeMap<String, u64> = BTreeMap::new();
        for ((_, amount), info) in largest.iter().zip(&infos) {
            if let Some(owner) = info.pointer("/data/parsed/info/owner").and_then(Value::as_str) {
                *by_owner.entry(owner.to_string()).or_insert(0) += amount;
            }
        }
        let owners: Vec<String> = by_owner.keys().cloned().collect();
        let owner_infos = accounts(rpc, &owners, lane).await?;
        credits += 1;
        let mut holders = Vec::new();
        for (owner, info) in owners.iter().zip(&owner_infos) {
            let program_owned = info.get("owner").and_then(Value::as_str).is_some_and(|program| program != SYSTEM_PROGRAM);
            let service = self.labels.get(owner).map(|l| l.name).or_else(|| program_owned.then(|| "pool or program account".to_string()));
            let entity = self.store.wallet(owner).await?.filter(|w| matches!(w.state, WalletState::Linked | WalletState::Candidate)).and_then(|w| w.entity_id);
            holders.push(Holder { owner: owner.clone(), amount: by_owner[owner], service, entity });
        }
        holders.sort_by_key(|h| std::cmp::Reverse(h.amount));

        let traders: Vec<&Holder> = holders.iter().filter(|h| h.service.is_none()).collect();
        let mut ties: Vec<(String, String, String)> = Vec::new();
        // Same entity.
        for (i, a) in traders.iter().enumerate() {
            for b in &traders[i + 1..] {
                if let (Some(x), Some(y)) = (a.entity, b.entity)
                    && x == y
                {
                    ties.push((a.owner.clone(), b.owner.clone(), format!("both wallets of E#{x}")));
                }
            }
        }
        // Who funded each holder: known answers first, then a bounded number of lookups.
        let mut funder_of: BTreeMap<String, String> = BTreeMap::new();
        let mut looked_up = 0;
        for holder in &traders {
            let known = self.store.doc("funder", &holder.owner).await?;
            let funder = match known {
                Some(doc) => doc.get("funder").and_then(Value::as_str).map(str::to_string),
                None if looked_up < TRACE_HOLDERS => {
                    looked_up += 1;
                    credits += 4;
                    let found = self.first_funder(&holder.owner, lane).await?;
                    let doc = match &found {
                        Some(f) => json!({ "funder": f.funder, "lamports": f.lamports, "at": f.at, "signature": f.signature }),
                        None => json!({ "funder": null }),
                    };
                    self.store.put_doc("funder", &holder.owner, &doc, self.now()).await?;
                    found.map(|f| f.funder)
                }
                None => None,
            };
            if let Some(funder) = funder {
                funder_of.insert(holder.owner.clone(), funder);
            }
        }
        for (i, a) in traders.iter().enumerate() {
            let Some(funder) = funder_of.get(&a.owner) else { continue };
            // An exchange or a service funds thousands of strangers: not a tie.
            if self.labels.get(funder).is_some() {
                continue;
            }
            if traders.iter().any(|h| &h.owner == funder) {
                ties.push((a.owner.clone(), funder.clone(), "one funded the other".to_string()));
            }
            for b in &traders[i + 1..] {
                if funder_of.get(&b.owner) == Some(funder) {
                    ties.push((a.owner.clone(), b.owner.clone(), format!("same funder {}", crate::util::short(funder))));
                }
            }
        }
        // Transfers between holders the bot has on record.
        for holder in &traders {
            for transfer in self.store.transfers_of_wallet(&holder.owner, 100).await? {
                if traders.iter().any(|h| h.owner == transfer.counterparty) {
                    ties.push((holder.owner.clone(), transfer.counterparty.clone(), "a transfer between them".to_string()));
                }
            }
        }
        let clusters = cluster(&holders, &ties);
        let report = MapReport { mint: mint.to_string(), supply, holders, clusters, credits };
        // Kept briefly, so the trader's token screen can use it without paying again.
        self.store.put_doc("map", mint, &json!({ "at": self.now(), "largest_bps": report.largest_cluster_bps(), "clusters": report.clusters.len() }), self.now()).await?;
        Ok(report)
    }
}
