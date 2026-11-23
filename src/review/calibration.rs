//! Calibration. Proof arrives late, which makes it a free answer key: every
//! wallet's score is stored when it is first proposed, and when proof or an
//! admin settles it, that score and the signals behind it are graded.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::app::App;
use crate::identity::score::{KINDS, kind};
use crate::model::*;
use crate::store::StoreResult;

/// Points per unit of log-odds: a signal right nine times in ten is worth
/// the link threshold by itself.
const SCALE: f64 = 31.86;
const MIN_SAMPLES: u32 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub name: String,
    pub right: u32,
    pub wrong: u32,
    pub current: Option<i32>,
    pub proposed: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub bands: Vec<Line>,
    pub kinds: Vec<Line>,
    /// Wallets settled by proof: the size of the answer key.
    pub proofs: i64,
    pub settled: u32,
    pub open: u32,
}

/// Measured points for a signal: its log-odds of being right, scaled, with
/// one imaginary right and one imaginary wrong so small samples stay modest.
pub fn proposed_points(right: u32, wrong: u32) -> Option<i32> {
    if right + wrong < MIN_SAMPLES {
        return None;
    }
    let p = (right as f64 + 1.0) / ((right + wrong) as f64 + 2.0);
    Some(((SCALE * (p / (1.0 - p)).ln()).round() as i32).clamp(-25, 60))
}

fn band_of(score: i64) -> &'static str {
    match score {
        i64::MIN..=49 => "40 to 49",
        50..=59 => "50 to 59",
        60..=69 => "60 to 69",
        _ => "70 and up",
    }
}

impl App {
    pub async fn calibration(&self) -> StoreResult<Table> {
        let weights = self.weights().await;
        let mut bands: BTreeMap<&'static str, (u32, u32)> = BTreeMap::new();
        let mut kinds: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        let (mut settled, mut open) = (0, 0);
        for (address, snapshot) in self.store.docs("snapshot", "", 10_000).await? {
            let Some(entity) = snapshot.get("entity").and_then(Value::as_i64) else { continue };
            let Some(wallet) = self.store.wallet(&address).await? else { continue };
            let confirmed = wallet.state == WalletState::Linked && matches!(wallet.linked_by, Some(LinkedBy::Proof | LinkedBy::Admin));
            // Right: proof or an admin tied it to the entity it was proposed for.
            // Wrong: an admin said no, or proof tied it to someone else.
            let right = match (confirmed, wallet.entity_id == Some(entity), wallet.state) {
                (true, true, _) => true,
                (true, false, _) | (_, true, WalletState::Rejected) => false,
                _ => {
                    open += 1;
                    continue;
                }
            };
            settled += 1;
            let tally = |slot: &mut (u32, u32)| if right { slot.0 += 1 } else { slot.1 += 1 };
            tally(bands.entry(band_of(snapshot.get("score").and_then(Value::as_i64).unwrap_or(0))).or_default());
            let mut seen: Vec<String> = snapshot.get("kinds").and_then(Value::as_array).map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
            // A review that said "link" is graded with the scripts' signals.
            let reviewed = self.store.evidence_of_wallet(&address).await?.iter().any(|e| e.entity_id == entity && e.kind == kind::AI_REVIEW && e.points > 0);
            if reviewed {
                seen.push(kind::AI_REVIEW.to_string());
            }
            seen.sort();
            seen.dedup();
            for name in seen {
                tally(kinds.entry(name).or_default());
            }
        }
        let line = |name: &str, (right, wrong): (u32, u32), scored: bool| Line {
            name: name.to_string(),
            right,
            wrong,
            current: scored.then(|| weights.points(name)),
            proposed: if scored { proposed_points(right, wrong) } else { None },
        };
        Ok(Table {
            bands: bands.into_iter().map(|(name, counts)| line(name, counts, false)).collect(),
            kinds: kinds.into_iter().filter(|(name, _)| KINDS.iter().any(|(k, ..)| k == name)).map(|(name, counts)| line(&name, counts, name != kind::DEPOSIT_PROOF)).collect(),
            proofs: self.proof_count().await,
            settled,
            open,
        })
    }

    /// Puts the measured points in force. Returns `(kind, old, new)` for each change.
    pub async fn apply_weights(self: &std::sync::Arc<Self>) -> StoreResult<Vec<(String, i32, i32)>> {
        let table = self.calibration().await?;
        let mut current = self.weights().await;
        let mut changed = Vec::new();
        for line in table.kinds {
            if let (Some(old), Some(new)) = (line.current, line.proposed)
                && old != new
            {
                current.0.insert(line.name.clone(), new);
                // Fingerprint and review rows carry their own computed points.
                if line.name != kind::FINGERPRINT && line.name != kind::AI_REVIEW {
                    self.store.repoint_evidence(&line.name, new).await?;
                }
                changed.push((line.name, old, new));
            }
        }
        if !changed.is_empty() {
            self.store.put_doc("weights", "current", &json!(current.0), self.now()).await?;
            for wallet in self.store.wallets_in_state(WalletState::Candidate).await? {
                if let Some(entity) = wallet.entity_id {
                    self.rejudge(&wallet.address, entity).await?;
                }
            }
        }
        Ok(changed)
    }
}
