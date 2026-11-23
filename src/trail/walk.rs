//! The grandfunder walk. From a target wallet, inside a time range, total
//! the inflow and outflow of every counterparty, follow the largest flows
//! hop by hop, and stop where the money reaches an exchange.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use serde_json::{Value, json};

use super::{JobEnd, JobResult, Sample};
use crate::app::App;
use crate::bot::cards;
use crate::bot::telegram::send;
use crate::identity::Unique;
use crate::identity::score::kind;
use crate::model::*;
use crate::obs::budget::Lane;

const SAMPLE_SIGNATURES: usize = 60;
const SAMPLE_TXS: usize = 30;
/// Counterparties followed from each wallet, largest flow first.
const BRANCH: usize = 6;
/// More SOL counterparties than this in one sample of 30 transactions is a service.
const HUB_COUNTERPARTIES: usize = 20;

/// A deposit address the walk reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundDeposit {
    pub address: String,
    pub exchange: String,
    pub hops: u32,
    pub senders: Vec<String>,
    /// Passed the sender limit of the deposit test.
    pub personal: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalkReport {
    pub target: String,
    pub days: i64,
    pub min_lamports: u64,
    pub visited: usize,
    pub credits: u64,
    /// The walk stopped at a limit, not because it ran out of trail.
    pub cut_short: Option<String>,
    pub deposits: Vec<FoundDeposit>,
    /// `(wallet, exchange, lamports)`: withdrawals that funded a wallet on the trail.
    pub withdrawals: Vec<(String, String, u64)>,
    /// `(from, to, lamports, hops)`: the flows that were followed.
    pub edges: Vec<(String, String, u64, u32)>,
}

/// SOL in and out per counterparty, as `(inflow, outflow)` in lamports.
pub fn totals(sample: &Sample) -> BTreeMap<String, (u64, u64)> {
    let mut out: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for flow in sample.flows.iter().filter(|f| f.mint.is_none()) {
        let entry = out.entry(flow.counterparty.clone()).or_default();
        match flow.direction {
            Direction::In => entry.0 += flow.amount_raw,
            Direction::Out => entry.1 += flow.amount_raw,
        }
    }
    out
}

impl App {
    pub async fn walk(self: &Arc<Self>, target: &str, days: i64, min_lamports: u64) -> Result<WalkReport, super::JobError> {
        let settings = &self.settings;
        let since = self.now() - days * 86_400;
        let mut report = WalkReport { target: target.to_string(), days, min_lamports, ..WalkReport::default() };
        let mut seen: BTreeSet<String> = BTreeSet::from([target.to_string()]);
        let mut queue: VecDeque<(String, u32)> = VecDeque::from([(target.to_string(), 0)]);

        while let Some((address, hops)) = queue.pop_front() {
            if report.visited >= settings.walk_wallets {
                report.cut_short = Some(format!("the wallet limit of {}", settings.walk_wallets));
                break;
            }
            if report.credits >= settings.walk_credits {
                report.cut_short = Some(format!("the credit cap of {}", settings.walk_credits));
                break;
            }
            let sample = self.sample_wallet(&address, SAMPLE_SIGNATURES, SAMPLE_TXS, Some(since), Lane::Forensics).await?;
            report.visited += 1;
            report.credits += sample.credits;

            // Is this wallet itself a deposit address? Then the trail ends here.
            if hops > 0
                && let Some(exchange) = self.swept_to_exchange(&sample)
            {
                let senders: Vec<String> = sample.senders().into_iter().map(str::to_string).collect();
                let personal = senders.len() <= settings.deposit_max_senders;
                report.deposits.push(FoundDeposit { address, exchange, hops, senders, personal });
                continue;
            }
            let mut flows: Vec<(String, u64, u64)> = totals(&sample).into_iter().map(|(who, (inflow, outflow))| (who, inflow, outflow)).collect();
            flows.sort_by_key(|(_, inflow, outflow)| std::cmp::Reverse(inflow + outflow));
            if flows.len() > HUB_COUNTERPARTIES {
                // Pays or collects from a crowd: a service. Not followed, and remembered.
                self.learn_hub(&address, flows.len()).await;
                continue;
            }
            let mut followed = 0;
            for (who, inflow, outflow) in flows {
                if inflow + outflow < min_lamports {
                    continue;
                }
                if let Some(exchange) = self.exchange_of(&who) {
                    if inflow > 0 {
                        report.withdrawals.push((address.clone(), exchange, inflow));
                    }
                    continue;
                }
                if self.labels.get(&who).is_some() {
                    continue;
                }
                if inflow > 0 {
                    report.edges.push((who.clone(), address.clone(), inflow, hops + 1));
                }
                if outflow > 0 {
                    report.edges.push((address.clone(), who.clone(), outflow, hops + 1));
                }
                if hops < settings.walk_hops && followed < BRANCH && seen.insert(who.clone()) {
                    queue.push_back((who, hops + 1));
                    followed += 1;
                }
            }
        }
        Ok(report)
    }

    /// Runs a walk for whoever asked, tags what it found and reports back.
    pub(crate) async fn job_walk(self: &Arc<Self>, job: &Job) -> JobResult {
        let target = job.subject.as_str();
        let days = job.payload.get("days").and_then(Value::as_i64).unwrap_or(self.settings.walk_days);
        let min_lamports = job.payload.get("min_lamports").and_then(Value::as_u64).unwrap_or(self.settings.walk_min_lamports);
        let report = self.walk(target, days, min_lamports).await?;

        // Tag what was found, when the target belongs to an entity.
        let entity = self.store.wallet(target).await?.filter(|w| w.state == WalletState::Linked).and_then(|w| w.entity_id);
        let mut tagged = 0;
        if let Some(entity) = entity {
            for found in &report.deposits {
                if self.store.deposit(&found.address).await?.is_some() {
                    continue;
                }
                // One hop out, the target paid in itself: the full deposit
                // test applies and the address becomes a tripwire. Further
                // out, the address is someone's on the trail, not provably
                // the target's, so its senders are leads and not proof.
                let direct = found.hops == 1 && found.senders.iter().any(|s| s == target);
                let status = match (direct, found.personal) {
                    (true, true) => DepositStatus::Qualified,
                    (_, false) => DepositStatus::Rejected,
                    (false, true) => DepositStatus::Pending,
                };
                let deposit = DepositAddr { address: found.address.clone(), entity_id: entity, exchange: found.exchange.clone(), status, senders: found.senders.len() as i32, first_seen: self.now() };
                self.store.put_deposit(&deposit).await?;
                tagged += 1;
                for sender in found.senders.iter().filter(|s| s.as_str() != target) {
                    match status {
                        DepositStatus::Qualified => self.deposit_proof(sender, &deposit, "").await?,
                        DepositStatus::Pending => {
                            self.add_evidence(
                                sender,
                                entity,
                                kind::SHARED_SINK,
                                None,
                                Vec::new(),
                                json!({ "sink": found.address, "exchange": found.exchange, "via": "walk" }),
                                Unique::Key(found.address.clone()),
                            )
                            .await?;
                        }
                        DepositStatus::Rejected => {}
                    }
                }
            }
            if tagged > 0 {
                self.refresh.notify_one();
            }
        }
        if let Some(chat) = job.payload.get("chat").and_then(Value::as_i64) {
            send(self.tg.as_ref(), chat, &cards::walk_card(&report, entity, tagged)).await;
        }
        self.store.put_doc("walk", target, &json!({ "at": self.now(), "visited": report.visited, "credits": report.credits, "deposits": report.deposits.iter().map(|d| json!({ "address": d.address, "exchange": d.exchange, "hops": d.hops, "senders": d.senders.len() })).collect::<Vec<_>>() }), self.now()).await?;
        Ok(JobEnd::Done)
    }
}
