//! Behaviour signals: how a wallet builds its transactions and when it
//! trades. Pure. These can raise a wallet to candidate; they can never
//! link one by themselves.

use std::collections::BTreeMap;

use crate::model::Fingerprint;

const ROTATION_WINDOW_S: i64 = 6 * 3_600;

/// The commonest value, when at least two trades share it.
fn mode<T: Ord + Clone>(values: impl Iterator<Item = T>) -> Option<T> {
    let mut counts: BTreeMap<T, usize> = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    counts.into_iter().filter(|(_, n)| *n >= 2).max_by_key(|(_, n)| *n).map(|(value, _)| value)
}

/// Up to 15 points for building transactions the way the entity does:
/// same trading app, same compute-unit price and limit, same tip. Exact
/// values, because apps and bots use presets. Zero when either side has
/// fewer than two trades, or the candidate's trades are signed by an app
/// (then the fingerprint is the app's, shared by all its users).
pub fn fingerprint_points(candidate: &[Fingerprint], entity: &[Fingerprint]) -> i32 {
    if candidate.len() < 2 || entity.len() < 2 {
        return 0;
    }
    if candidate.iter().filter(|p| !p.self_paid || !p.wallet_signed).count() * 2 > candidate.len() {
        return 0;
    }
    let mut points = 0;
    let app = |prints: &[Fingerprint]| mode(prints.iter().filter_map(|p| p.app.clone()));
    if let (Some(a), Some(b)) = (app(candidate), app(entity))
        && a == b
    {
        points += 5;
    }
    let price = |prints: &[Fingerprint]| mode(prints.iter().filter_map(|p| p.cu_price));
    if let (Some(a), Some(b)) = (price(candidate), price(entity))
        && a == b
    {
        points += 4;
    }
    let limit = |prints: &[Fingerprint]| mode(prints.iter().filter_map(|p| p.cu_limit));
    if let (Some(a), Some(b)) = (limit(candidate), limit(entity))
        && a == b
    {
        points += 2;
    }
    let tip = |prints: &[Fingerprint]| mode(prints.iter().filter(|p| p.tip_lamports > 0).map(|p| (p.tip_family.clone(), p.tip_lamports)));
    if let (Some(a), Some(b)) = (tip(candidate), tip(entity))
        && a == b
    {
        points += 4;
    }
    points
}

/// Rotation: one of the entity's wallets made its last trade within six
/// hours of this wallet's first, and has been quiet for six hours since.
pub fn rotated(candidate_first: i64, linked_last: &[i64], now: i64) -> bool {
    linked_last.iter().any(|last| (candidate_first - last).abs() <= ROTATION_WINDOW_S && now - last >= ROTATION_WINDOW_S)
}
