//! Attaching wallets to entities. Every change goes through here: a row of
//! evidence is added (or voided), the wallet is judged again from all of
//! its live rows, and a change of state is announced to the followers.

pub mod behaviour;
pub mod score;

use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::App;
use crate::bot::cards;
use crate::bot::telegram::{Card, send};
use crate::model::*;
use crate::store::StoreResult;
use score::{GREY_FROM, LINK_AT, Verdict, Weights, family_of, judge, kind};

/// How a new row relates to rows of the same kind already there.
pub enum Unique {
    /// One row per key (a mint, a deposit address, a funding transaction).
    Key(String),
    /// One live row of this kind: the new one replaces the old.
    Replace,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub before: Option<Wallet>,
    pub after: Wallet,
    pub verdict: Verdict,
}

impl Change {
    pub fn became(&self, state: WalletState) -> bool {
        self.after.state == state && self.before.as_ref().map(|w| (w.state, w.entity_id)) != Some((state, self.after.entity_id))
    }
}

impl App {
    /// The points table in force: starting values with approved changes.
    pub async fn weights(&self) -> Weights {
        let mut weights = Weights::default();
        if let Ok(Some(Value::Object(map))) = self.store.doc("weights", "current").await {
            for (kind_name, points) in map {
                if let Some(points) = points.as_i64() {
                    weights.0.insert(kind_name, points as i32);
                }
            }
        }
        weights
    }

    /// Accounts that get an entity's cards: following, and not paused.
    pub async fn followers(&self, entity: EntityId) -> StoreResult<Vec<(Account, FollowMode)>> {
        let mut out = Vec::new();
        for follow in self.store.follows_of_entity(entity).await? {
            if follow.mode == FollowMode::Off {
                continue;
            }
            if let Some(account) = self.store.account(follow.account).await?.filter(|a| !a.paused) {
                out.push((account, follow.mode));
            }
        }
        Ok(out)
    }

    pub async fn notify_followers(&self, entity: EntityId, card: &Card) {
        let Ok(followers) = self.followers(entity).await else { return };
        for (account, _) in followers {
            send(self.tg.as_ref(), account.id, card).await;
        }
    }

    /// Records one reason a wallet belongs to an entity and judges the
    /// wallet again. `None` when the row was already there, or the address
    /// is a labelled service.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_evidence(
        self: &Arc<Self>,
        wallet: &str,
        entity: EntityId,
        kind_name: &str,
        points: Option<i32>,
        signatures: Vec<String>,
        mut details: Value,
        unique: Unique,
    ) -> StoreResult<Option<Change>> {
        if self.labels.get(wallet).is_some() {
            return Ok(None);
        }
        let points = match points {
            Some(points) => points,
            None => self.weights().await.points(kind_name),
        };
        let live: Vec<Evidence> = self.store.evidence_of_wallet(wallet).await?.into_iter().filter(|e| e.entity_id == entity && e.kind == kind_name && !e.voided).collect();
        match &unique {
            Unique::Key(key) => {
                if live.iter().any(|e| e.details.get("key").and_then(Value::as_str) == Some(key)) {
                    return Ok(None);
                }
                details["key"] = json!(key);
            }
            Unique::Replace => {
                if live.len() == 1 && live[0].points == points {
                    return Ok(None);
                }
                for row in &live {
                    self.store.void_evidence_row(row.id).await?;
                }
            }
        }
        let source = if kind_name == kind::AI_REVIEW {
            "ai"
        } else if family_of(kind_name) == score::family::MANUAL {
            "admin"
        } else {
            "script"
        };
        self.store
            .add_evidence(&NewEvidence {
                wallet: wallet.to_string(),
                entity_id: entity,
                kind: kind_name.to_string(),
                family: family_of(kind_name).to_string(),
                points,
                signatures,
                details,
                source: source.to_string(),
                observed_at: self.now(),
            })
            .await?;
        self.rejudge(wallet, entity).await
    }

    /// Judges a wallet from its live evidence for one entity, saves the
    /// result and announces a change of state.
    pub async fn rejudge(self: &Arc<Self>, address: &str, entity_id: EntityId) -> StoreResult<Option<Change>> {
        let Some(entity) = self.store.entity(entity_id).await? else { return Ok(None) };
        let rows: Vec<Evidence> = self.store.evidence_of_wallet(address).await?.into_iter().filter(|e| e.entity_id == entity_id).collect();
        let before = self.store.wallet(address).await?;
        let was_rejected = before.as_ref().is_some_and(|w| w.state == WalletState::Rejected && w.entity_id == Some(entity_id));
        let verdict = judge(&rows, was_rejected);

        // A wallet belongs to one entity. Evidence for a second one moves it
        // only when the first claim is weaker, and never away from a link.
        if let Some(current) = before.as_ref().filter(|w| w.entity_id.is_some_and(|e| e != entity_id) && w.state != WalletState::Rejected) {
            let other = current.entity_id.unwrap_or_default();
            if current.state == WalletState::Linked {
                if verdict.state == WalletState::Linked {
                    self.suggest_merge(other, entity_id, address).await;
                }
                return Ok(None);
            }
            if verdict.score <= current.score {
                return Ok(None);
            }
        }

        let after = Wallet {
            address: address.to_string(),
            entity_id: Some(entity_id),
            state: verdict.state,
            score: verdict.score,
            linked_by: verdict.linked_by,
            first_seen: before.as_ref().map(|w| w.first_seen).unwrap_or_else(|| self.now()),
            last_signature: before.as_ref().and_then(|w| w.last_signature.clone()),
        };
        if before.as_ref() != Some(&after) {
            self.store.put_wallet(&after).await?;
        }
        let change = Change { before, after, verdict };
        let live: Vec<Evidence> = rows.into_iter().filter(|e| !e.voided).collect();

        if change.became(WalletState::Candidate) {
            // The score when first proposed is what calibration grades later.
            let kinds: Vec<&str> = live.iter().map(|e| e.kind.as_str()).collect();
            if self.store.doc("snapshot", address).await?.is_none() {
                self.store.put_doc("snapshot", address, &json!({ "entity": entity_id, "score": change.verdict.score, "kinds": kinds, "at": self.now() }), self.now()).await?;
            }
            self.notify_followers(entity_id, &cards::candidate_card(&entity, &change.after, &live, &change.verdict)).await;
            self.start_watching(address);
            self.enqueue("trace_back", address, Some(entity_id), json!({ "hops": 2 }), 0).await;
            log::info!("{address} is a candidate for {} at {}", entity.tag(), change.verdict.score);
        } else if change.became(WalletState::Linked) {
            self.notify_followers(entity_id, &cards::linked_card(&entity, &change.after, &live, &change.verdict)).await;
            self.start_watching(address);
            log::info!("{address} linked to {} by {:?}", entity.tag(), change.after.linked_by);
        } else if change.before.as_ref().is_some_and(|w| matches!(w.state, WalletState::Linked | WalletState::Candidate)) && !matches!(change.after.state, WalletState::Linked | WalletState::Candidate)
        {
            self.refresh.notify_one();
        }
        // The grey band is the reviewer's: scores the scripts cannot settle.
        if change.after.state == WalletState::Candidate && (GREY_FROM..LINK_AT).contains(&change.verdict.score) && self.reviewer().is_some() && !live.iter().any(|e| e.kind == kind::AI_REVIEW) {
            self.enqueue("review", address, Some(entity_id), json!({}), 0).await;
        }
        Ok(Some(change))
    }

    async fn suggest_merge(&self, a: EntityId, b: EntityId, address: &str) {
        let key = format!("{}:{}", a.min(b), a.max(b));
        if matches!(self.store.doc("mergehint", &key).await, Ok(Some(_))) {
            return;
        }
        let _ = self.store.put_doc("mergehint", &key, &json!({ "wallet": address }), self.now()).await;
        let text =
            format!("Wallet <code>{address}</code> is linked to E#{a} and now has linking evidence for E#{b}. They may be one trader.\n<code>/merge {} {}</code> joins them.", a.max(b), a.min(b));
        for admin in &self.settings.admin_ids {
            send(self.tg.as_ref(), *admin, &Card::text(text.clone())).await;
        }
    }

    /// An admin's "yes, that is them".
    pub async fn confirm_wallet(self: &Arc<Self>, address: &str, by: AccountId) -> StoreResult<Option<Change>> {
        let Some(wallet) = self.store.wallet(address).await? else { return Ok(None) };
        let Some(entity) = wallet.entity_id else { return Ok(None) };
        if wallet.state == WalletState::Rejected {
            // Confirming undoes an earlier "not them".
            self.store.put_wallet(&Wallet { state: WalletState::Seen, ..wallet }).await?;
        }
        self.add_evidence(address, entity, kind::ADMIN_CONFIRM, Some(100), Vec::new(), json!({ "by": by }), Unique::Replace).await
    }

    /// An admin's "not them": every row is voided and the wallet is kept
    /// as rejected, so it is not proposed for this entity again.
    pub async fn reject_wallet(self: &Arc<Self>, address: &str) -> StoreResult<Option<EntityId>> {
        let Some(wallet) = self.store.wallet(address).await? else { return Ok(None) };
        let Some(entity) = wallet.entity_id else { return Ok(None) };
        self.store.void_evidence(address, entity).await?;
        self.store.put_wallet(&Wallet { state: WalletState::Rejected, score: 0, linked_by: None, ..wallet }).await?;
        self.refresh.notify_one();
        Ok(Some(entity))
    }

    /// Two entities turn out to be one trader: everything of `from` moves
    /// to `into`, and `from` is retired.
    pub async fn merge_entities(self: &Arc<Self>, from: EntityId, into: EntityId) -> StoreResult<usize> {
        let wallets = self.store.wallets_of(from).await?;
        for wallet in &wallets {
            let mut moved = wallet.clone();
            moved.entity_id = Some(into);
            if moved.linked_by == Some(LinkedBy::Root) {
                moved.linked_by = Some(LinkedBy::Admin);
            }
            self.store.put_wallet(&moved).await?;
        }
        self.store.move_evidence(from, into).await?;
        for deposit in self.store.deposits_of(from).await? {
            self.store.put_deposit(&DepositAddr { entity_id: into, ..deposit }).await?;
        }
        let already: Vec<AccountId> = self.store.follows_of_entity(into).await?.into_iter().map(|f| f.account).collect();
        for follow in self.store.follows_of_entity(from).await? {
            if !already.contains(&follow.account) {
                self.store.set_follow(follow.account, into, follow.mode).await?;
            }
        }
        self.store.set_entity_status(from, EntityStatus::Retired).await?;
        self.refresh.notify_one();
        Ok(wallets.len())
    }

    /// A wallet turns out to be someone else: it becomes the root of a new entity.
    pub async fn split_wallet(self: &Arc<Self>, address: &str, by: AccountId) -> StoreResult<Option<Entity>> {
        let Some(wallet) = self.store.wallet(address).await? else { return Ok(None) };
        let Some(old) = wallet.entity_id else { return Ok(None) };
        let now = self.now();
        self.store.void_evidence(address, old).await?;
        let entity = self.store.create_entity(&format!("split from E#{old}"), by, now).await?;
        self.store.put_wallet(&Wallet { entity_id: Some(entity.id), state: WalletState::Linked, score: 100, linked_by: Some(LinkedBy::Root), ..wallet }).await?;
        self.store
            .add_evidence(&NewEvidence {
                wallet: address.to_string(),
                entity_id: entity.id,
                kind: kind::ROOT.into(),
                family: score::family::MANUAL.into(),
                points: 100,
                signatures: Vec::new(),
                details: json!({ "by": by, "split_from": old }),
                source: "admin".into(),
                observed_at: now,
            })
            .await?;
        self.store.set_follow(by, entity.id, FollowMode::Cards).await?;
        self.refresh.notify_one();
        Ok(Some(entity))
    }

    /// "Entity lost": quiet for five days with no candidate in sight. The
    /// followers are asked once whether to retire it or keep watching.
    pub async fn lost_check(self: &Arc<Self>) -> StoreResult<usize> {
        const QUIET_S: i64 = 5 * 86_400;
        let now = self.now();
        let mut told = 0;
        for entity in self.store.entities().await?.into_iter().filter(|e| e.status == EntityStatus::Tracking && now - e.created_at >= QUIET_S) {
            let last = self.store.trades_of_entity(entity.id, 1).await?.first().map(|t| t.block_time.unwrap_or(t.seen_at)).unwrap_or(entity.created_at);
            let candidate = self.store.wallets_of(entity.id).await?.iter().any(|w| w.state == WalletState::Candidate);
            let key = entity.id.to_string();
            let asked = self.store.doc("lost", &key).await?.and_then(|d| d.get("at").and_then(Value::as_i64)).is_some_and(|at| now - at < QUIET_S);
            if now - last < QUIET_S || candidate || asked {
                continue;
            }
            self.store.put_doc("lost", &key, &json!({ "at": now }), now).await?;
            self.notify_followers(entity.id, &cards::lost_card(&entity, (now - last) / 86_400)).await;
            told += 1;
        }
        Ok(told)
    }

    /// How many wallets have been settled by proof: the size of the answer key.
    pub async fn proof_count(&self) -> i64 {
        self.store.get_state("proofs").await.ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0)
    }
}
