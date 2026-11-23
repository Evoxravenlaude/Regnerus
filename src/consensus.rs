//! Consensus counts entities, not wallets. Five burners of one trader are
//! one vote, and entities that always buy together are one vote between them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::App;
use crate::bot::cards;
use crate::bot::telegram::send;
use crate::model::*;
use crate::store::StoreResult;

/// Two entities are one bloc when this share of the smaller one's buys were made together.
const BLOC_SHARE: f64 = 0.6;
const BLOC_MIN_BUYS: usize = 5;
const TOGETHER_S: i64 = 60;
const BLOC_HISTORY_S: i64 = 30 * 86_400;

/// Groups entities that co-buy most of the time. Maps each entity in a
/// bloc to the bloc's smallest id; an entity on its own is not in the map.
pub fn blocs(signals: &[Signal]) -> BTreeMap<EntityId, EntityId> {
    let mut buys: BTreeMap<EntityId, Vec<(&str, i64)>> = BTreeMap::new();
    for signal in signals.iter().filter(|s| s.side == Side::Buy) {
        buys.entry(signal.entity_id).or_default().push((signal.mint.as_str(), signal.first_at));
    }
    let ids: Vec<EntityId> = buys.keys().copied().collect();
    let mut parent: BTreeMap<EntityId, EntityId> = ids.iter().map(|id| (*id, *id)).collect();
    fn root(parent: &BTreeMap<EntityId, EntityId>, mut id: EntityId) -> EntityId {
        while parent[&id] != id {
            id = parent[&id];
        }
        id
    }
    for (i, a) in ids.iter().enumerate() {
        for b in &ids[i + 1..] {
            let (ours, theirs) = (&buys[a], &buys[b]);
            if ours.len() < BLOC_MIN_BUYS || theirs.len() < BLOC_MIN_BUYS {
                continue;
            }
            let together = ours.iter().filter(|(mint, at)| theirs.iter().any(|(m, t)| m == mint && (t - at).abs() <= TOGETHER_S)).count();
            if together as f64 >= BLOC_SHARE * ours.len().min(theirs.len()) as f64 {
                let (ra, rb) = (root(&parent, *a), root(&parent, *b));
                parent.insert(ra.max(rb), ra.min(rb));
            }
        }
    }
    let roots: Vec<(EntityId, EntityId)> = ids.iter().map(|id| (*id, root(&parent, *id))).collect();
    roots.iter().filter(|(_, bloc)| roots.iter().filter(|(_, other)| other == bloc).count() > 1).copied().collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// `(entity, wallets that bought)`, in the order they bought.
    pub entities: Vec<(EntityId, usize)>,
    /// Entities counted once per bloc.
    pub independent: usize,
}

/// Counts the entities that bought within the window ending now. `voters`
/// limits the count to entities one user follows; crowded buys do not count.
pub fn tally(buys: &[Signal], blocs: &BTreeMap<EntityId, EntityId>, crowded: &BTreeSet<EntityId>, voters: Option<&BTreeSet<EntityId>>, since: i64) -> Tally {
    let mut entities: Vec<(EntityId, usize)> = Vec::new();
    for signal in buys.iter().filter(|s| s.side == Side::Buy && s.last_at >= since) {
        if crowded.contains(&signal.entity_id) || voters.is_some_and(|v| !v.contains(&signal.entity_id)) {
            continue;
        }
        match entities.iter_mut().find(|(id, _)| *id == signal.entity_id) {
            Some(entry) => entry.1 = entry.1.max(signal.wallets.len()),
            None => entities.push((signal.entity_id, signal.wallets.len())),
        }
    }
    let independent: BTreeSet<EntityId> = entities.iter().map(|(id, _)| blocs.get(id).copied().unwrap_or(*id)).collect();
    Tally { independent: independent.len(), entities }
}

impl App {
    /// The user's copy settings, or the defaults.
    pub async fn trade_settings(&self, account: AccountId) -> TradeSettings {
        match self.store.doc("settings", &account.to_string()).await {
            Ok(Some(value)) => serde_json::from_value(value).unwrap_or_default(),
            _ => TradeSettings::default(),
        }
    }

    /// How many independent entities this user follows have bought the token inside their window.
    pub async fn consensus_for(&self, account: AccountId, mint: &str, settings: &TradeSettings) -> StoreResult<Tally> {
        let now = self.now();
        let window = settings.window_min.max(1) as i64 * 60;
        let recent: Vec<Signal> = self.store.signals_since(now - window).await?.into_iter().filter(|s| s.mint == mint).collect();
        let voters: BTreeSet<EntityId> = self.store.follows_of_account(account).await?.into_iter().filter(|f| f.mode != FollowMode::Off).map(|f| f.entity_id).collect();
        let blocs = self.blocs().await?;
        let mut crowded = BTreeSet::new();
        for (key, _) in self.store.docs("crowd", &format!("{mint}:"), 200).await? {
            if let Some(entity) = key.split(':').nth(1).and_then(|e| e.parse().ok()) {
                crowded.insert(entity);
            }
        }
        Ok(tally(&recent, &blocs, &crowded, Some(&voters), now - window))
    }

    /// Which entities buy together so often that they count as one. Worked
    /// out from a month of signals and kept for ten minutes.
    pub async fn blocs(&self) -> StoreResult<BTreeMap<EntityId, EntityId>> {
        let now = self.now();
        if let Some((at, cached)) = self.blocs_cache.lock().as_ref()
            && now - at < 600
        {
            return Ok(cached.clone());
        }
        let fresh = blocs(&self.store.signals_since(now - BLOC_HISTORY_S).await?);
        *self.blocs_cache.lock() = Some((now, fresh.clone()));
        Ok(fresh)
    }

    /// After an entity's buy: tell each follower whose threshold the token
    /// has just reached. One alert per user and token per window.
    pub(crate) async fn check_consensus(self: &Arc<Self>, signal: &Signal) -> StoreResult<()> {
        let now = self.now();
        for (account, _) in self.followers(signal.entity_id).await? {
            let settings = self.trade_settings(account.id).await;
            let seen = self.consensus_for(account.id, &signal.mint, &settings).await?;
            if seen.independent < settings.min_entities.max(2) as usize {
                continue;
            }
            let key = format!("{}:{}", account.id, signal.mint);
            let window = settings.window_min.max(1) as i64 * 60;
            let told = self.store.doc("consensus", &key).await?.and_then(|d| d.get("at").and_then(Value::as_i64)).is_some_and(|at| now - at < window);
            if told {
                continue;
            }
            self.store.put_doc("consensus", &key, &json!({ "at": now, "independent": seen.independent }), now).await?;
            let mut names = Vec::new();
            for (id, wallets) in &seen.entities {
                if let Some(entity) = self.store.entity(*id).await? {
                    names.push((entity, *wallets));
                }
            }
            send(self.tg.as_ref(), account.id, &cards::consensus_card(&signal.mint, &names, seen.independent, settings.window_min)).await;
        }
        Ok(())
    }
}
