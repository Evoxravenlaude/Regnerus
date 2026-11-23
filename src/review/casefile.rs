//! The case file the reviewer reads. Built by scripts from the store, and
//! deliberately dull: addresses, numbers and fixed categories. Token names,
//! symbols and memos are written by strangers and could carry instructions,
//! so no free text from the chain goes in.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::app::App;
use crate::identity::score::family;
use crate::model::*;
use crate::store::StoreResult;

/// Keeps a string only when it is an address, a signature or a short plain word.
fn safe_text(text: &str) -> Option<String> {
    let plain = !text.is_empty() && text.len() <= 96 && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    plain.then(|| text.to_string())
}

/// Strips everything from a JSON value except numbers, booleans and safe strings.
pub fn sanitize(value: &Value) -> Value {
    match value {
        Value::Number(_) | Value::Bool(_) | Value::Null => value.clone(),
        Value::String(text) => safe_text(text).map(Value::String).unwrap_or(Value::Null),
        Value::Array(list) => Value::Array(list.iter().take(20).map(sanitize).collect()),
        Value::Object(map) => Value::Object(map.iter().filter_map(|(k, v)| Some((safe_text(k)?, sanitize(v)))).collect::<Map<_, _>>()),
    }
}

/// The commonest fingerprint values across some trades.
fn fingerprint_summary(trades: &[Trade]) -> Value {
    fn top<T: Ord + Clone + serde::Serialize>(values: Vec<T>) -> Value {
        let mut counts: BTreeMap<T, usize> = BTreeMap::new();
        for value in values {
            *counts.entry(value).or_insert(0) += 1;
        }
        counts.into_iter().max_by_key(|(_, n)| *n).map(|(v, n)| json!({ "value": v, "trades": n })).unwrap_or(Value::Null)
    }
    let prints: Vec<&Fingerprint> = trades.iter().map(|t| &t.fingerprint).collect();
    json!({
        "trades": trades.len(),
        "self_paid": prints.iter().filter(|p| p.self_paid).count(),
        "app": top(prints.iter().filter_map(|p| p.app.as_deref().and_then(safe_text)).collect()),
        "cu_price": top(prints.iter().filter_map(|p| p.cu_price).collect()),
        "cu_limit": top(prints.iter().filter_map(|p| p.cu_limit).collect()),
        "tip_lamports": top(prints.iter().filter(|p| p.tip_lamports > 0).map(|p| p.tip_lamports).collect()),
        "venues": top(trades.iter().map(|t| t.venue.as_str()).collect()),
    })
}

impl App {
    pub async fn case_file(&self, entity: &Entity, wallet: &str) -> StoreResult<Value> {
        let linked: Vec<Wallet> = self.store.wallets_of(entity.id).await?.into_iter().filter(|w| w.state == WalletState::Linked).collect();
        let theirs = self.store.trades_of_entity(entity.id, 60).await?;
        let mine = self.store.trades_of_wallet(wallet, 60).await?;
        let candidate = self.store.wallet(wallet).await?;
        let rows: Vec<Evidence> = self.store.evidence_of_wallet(wallet).await?.into_iter().filter(|e| e.entity_id == entity.id && !e.voided).collect();
        let row = |e: &Evidence| json!({ "id": format!("E{}", e.id), "kind": e.kind, "family": e.family, "points": e.points, "signatures": e.signatures.iter().take(3).collect::<Vec<_>>(), "details": sanitize(&e.details), "observed_at": e.observed_at });
        let funder = self.store.doc("funder", wallet).await?.map(|d| sanitize(&d)).unwrap_or(Value::Null);
        let funder_class = funder.get("funder").and_then(Value::as_str).map(|f| match self.labels.get(f) {
            Some(label) => label.class.as_str().to_string(),
            None => "plain_wallet".to_string(),
        });
        // How many other wallets the same windows fit: the fewer, the more a match means.
        let other_fits: u64 = rows.iter().filter_map(|e| e.details.get("fits").and_then(Value::as_u64)).map(|n| n.saturating_sub(1)).sum();
        let deposits: Vec<String> = self.store.deposits_of(entity.id).await?.into_iter().filter(|d| d.status == DepositStatus::Qualified).filter_map(|d| safe_text(&d.exchange)).collect();
        Ok(json!({
            "entity": {
                "id": entity.id,
                "linked_wallets": linked.len(),
                "fingerprint": fingerprint_summary(&theirs),
                "deposit_exchanges": deposits,
            },
            "candidate": {
                "address": wallet,
                "score": candidate.as_ref().map(|w| w.score).unwrap_or(0),
                "first_seen": candidate.as_ref().map(|w| w.first_seen).unwrap_or(0),
                "fingerprint": fingerprint_summary(&mine),
                "first_funder": funder,
                "first_funder_class": funder_class,
            },
            "evidence": rows.iter().filter(|e| e.family != family::NEGATIVE && e.family != family::REVIEW).map(row).collect::<Vec<_>>(),
            "counter_evidence": rows.iter().filter(|e| e.family == family::NEGATIVE).map(row).collect::<Vec<_>>(),
            "other_wallets_fitting_the_same_windows": other_fits,
        }))
    }
}
