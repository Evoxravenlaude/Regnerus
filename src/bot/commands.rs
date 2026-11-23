//! What people can ask of the bot, in a private chat.
//!
//! Access is by admin id or invite code, as in Yeks. Entities and evidence
//! are one shared graph for the whole circle; follows and pauses belong to
//! each user.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::App;
use crate::bot::cards::{self, TransferRow, WalletView};
use crate::bot::telegram::{Card, answer_callback, edit, send};
use crate::model::*;
use crate::store::StoreResult;
use crate::util::{esc, is_solana_address, short};

const PRIVATE_NOTICE: &str = "This bot is private. Ask for an invite code, then send <code>/start CODE</code>.";
const RECENT_TRADES: i64 = 8;

/// The entity id in `12` or `E#12`.
fn entity_id_of(text: &str) -> Option<EntityId> {
    let digits = text.trim().trim_start_matches(['E', 'e']).trim_start_matches('#');
    digits.parse::<EntityId>().ok().filter(|id| *id > 0)
}

/// A ten-character code from the process's random hasher keys. Good enough
/// for an invite that an admin hands to a friend; not a secret key.
fn invite_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut out = String::new();
    while out.len() < 10 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_usize(out.len());
        let mut bits = hasher.finish();
        for _ in 0..5 {
            out.push(ALPHABET[(bits % ALPHABET.len() as u64) as usize] as char);
            bits /= ALPHABET.len() as u64;
        }
    }
    out.truncate(10);
    out
}

fn problem(error: impl std::fmt::Display) -> Card {
    log::error!("command failed: {error}");
    Card::text("Something went wrong on my side. It is in the log.")
}

impl App {
    /// One update from `getUpdates`.
    pub async fn on_update(self: &Arc<Self>, update: &Value) {
        if let Some(message) = update.get("message") {
            self.on_message(message).await;
        } else if let Some(callback) = update.get("callback_query") {
            self.on_callback(callback).await;
        }
    }

    /// The account for this sender, creating it for an admin or on a valid
    /// invite code. `None` means no access.
    async fn admit(&self, user: i64, name: &str, text: &str) -> StoreResult<Option<Account>> {
        let now = self.now();
        if self.settings.admin_ids.contains(&user) {
            return Ok(Some(self.store.upsert_account(user, name, true, now).await?));
        }
        if self.store.account(user).await?.is_some() {
            // Keeps the name current, and drops the admin flag of someone
            // who is no longer in ADMIN_IDS.
            return Ok(Some(self.store.upsert_account(user, name, false, now).await?));
        }
        let mut words = text.split_whitespace();
        let command = words.next().unwrap_or("");
        if command.split('@').next() == Some("/start")
            && let Some(code) = words.next()
            && self.store.redeem_invite(&code.to_ascii_uppercase()).await?
        {
            log::info!("{user} joined with an invite code");
            return Ok(Some(self.store.upsert_account(user, name, false, now).await?));
        }
        Ok(None)
    }

    async fn on_message(self: &Arc<Self>, message: &Value) {
        if message.get("chat").and_then(|c| c.get("type")).and_then(Value::as_str) != Some("private") {
            return; // groups come later; cards go to DMs
        }
        let Some(user) = message.get("from").and_then(|f| f.get("id")).and_then(Value::as_i64) else { return };
        let Some(chat) = message.get("chat").and_then(|c| c.get("id")).and_then(Value::as_i64) else { return };
        let Some(text) = message.get("text").and_then(Value::as_str).map(str::trim) else { return };
        let from = message.get("from");
        let name = from
            .and_then(|f| f.get("username").and_then(Value::as_str).map(|u| format!("@{u}")))
            .or_else(|| from.and_then(|f| f.get("first_name")).and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();

        let account = match self.admit(user, &name, text).await {
            Ok(Some(account)) => account,
            Ok(None) => {
                send(self.tg.as_ref(), chat, &Card::text(PRIVATE_NOTICE)).await;
                return;
            }
            Err(error) => {
                send(self.tg.as_ref(), chat, &problem(error)).await;
                return;
            }
        };
        let card = self.run_command(&account, text).await;
        send(self.tg.as_ref(), chat, &card).await;
    }

    /// Runs one line of text as this account and returns the reply.
    pub async fn run_command(self: &Arc<Self>, account: &Account, text: &str) -> Card {
        let mut words = text.split_whitespace();
        let first = words.next().unwrap_or("");
        let args: Vec<&str> = words.collect();
        if !first.starts_with('/') {
            // A bare address is the commonest thing to send: track it.
            return if is_solana_address(first) {
                self.cmd_track(account, first, &args.join(" ")).await.unwrap_or_else(problem)
            } else {
                Card::text("Paste a Solana wallet address to track it, or send /help.")
            };
        }
        let command = first.split('@').next().unwrap_or(first).to_ascii_lowercase();
        let admin_only =
            matches!(command.as_str(), "/link" | "/unlink" | "/label" | "/unlabel" | "/labels" | "/code" | "/users" | "/merge" | "/split" | "/confirm" | "/review" | "/copyable" | "/kill" | "/unkill");
        if admin_only && !account.is_admin {
            return Card::text("That command is for admins.");
        }
        let result = match command.as_str() {
            "/start" | "/help" => Ok(cards::help_card(account.is_admin)),
            "/track" => match args.first() {
                Some(address) => self.cmd_track(account, address, &args[1..].join(" ")).await,
                None => Ok(Card::text("Usage: <code>/track ADDRESS label</code>")),
            },
            "/entities" => self.cmd_entities(account).await,
            "/entity" => match args.first().and_then(|a| entity_id_of(a)) {
                Some(id) => self.entity_view(account, id).await,
                None => Ok(Card::text("Usage: <code>/entity ID</code>")),
            },
            "/wallet" => match args.first() {
                Some(address) if is_solana_address(address) => self.cmd_wallet(address).await,
                _ => Ok(Card::text("Usage: <code>/wallet ADDRESS</code>")),
            },
            "/follow" => self.cmd_follow(account, &args).await,
            "/rename" => self.cmd_rename(account, &args).await,
            "/retire" => self.cmd_status(account, &args, EntityStatus::Retired).await,
            "/revive" => self.cmd_status(account, &args, EntityStatus::Tracking).await,
            "/pause" => self.store.set_paused(account.id, true).await.map(|_| Card::text("Paused. No cards until you send /resume.")),
            "/resume" => self.store.set_paused(account.id, false).await.map(|_| Card::text("Resumed.")),
            "/health" => self.cmd_health().await,
            "/budget" => Ok(cards::budget_card(&self.budget.snapshot())),
            "/link" => self.cmd_link(account, &args).await,
            "/unlink" => self.cmd_unlink(account, &args).await,
            "/label" => self.cmd_label(&args).await,
            "/unlabel" => self.cmd_unlabel(&args).await,
            "/labels" => Ok(cards::labels_card(&self.labels.all())),
            "/code" => self.cmd_code(account, &args).await,
            "/users" => self.store.accounts().await.map(|accounts| cards::users_card(&accounts)),
            other => self.more_command(account, other, &args).await,
        };
        result.unwrap_or_else(problem)
    }

    async fn follow_mode(&self, account: AccountId, entity: EntityId) -> StoreResult<FollowMode> {
        Ok(self.store.follows_of_account(account).await?.into_iter().find(|f| f.entity_id == entity).map(|f| f.mode).unwrap_or(FollowMode::Off))
    }

    /// The entity card as this account sees it.
    pub async fn entity_view(&self, account: &Account, id: EntityId) -> StoreResult<Card> {
        let Some(entity) = self.store.entity(id).await? else { return Ok(Card::text(format!("There is no entity E#{id}."))) };
        let wallets = self.store.wallets_of(id).await?;
        let follow = self.follow_mode(account.id, id).await?;
        let trades = self.store.trades_of_entity(id, RECENT_TRADES).await?;
        let mut card = cards::entity_card(&entity, &wallets, follow, &trades, self.now());
        let deposits = cards::deposits_lines(&self.store.deposits_of(id).await?);
        if !deposits.is_empty() {
            card.text = format!("{}\n\n{}", card.text, deposits.join("\n"));
        }
        Ok(card)
    }

    /// Reads a newly watched wallet's recent history in the background and
    /// brings the stream's subscriptions up to date.
    pub(crate) fn start_watching(self: &Arc<Self>, address: &str) {
        self.refresh.notify_one();
        let app = self.clone();
        let address = address.to_string();
        tokio::spawn(async move {
            let read = app.backfill_wallet(&address).await;
            log::info!("read {read} past transactions of {address}");
        });
    }

    async fn cmd_track(self: &Arc<Self>, account: &Account, address: &str, label: &str) -> StoreResult<Card> {
        if !is_solana_address(address) {
            return Ok(Card::text("That is not a Solana address (32 bytes of base58)."));
        }
        if let Some(label) = self.labels.get(address) {
            return Ok(Card::text(format!("That address is labelled as a service: {} ({}). Services are not tracked as entities.", esc(&label.name), label.class)));
        }
        if let Some(existing) = self.store.wallet(address).await?
            && let Some(entity_id) = existing.entity_id
        {
            if existing.state == WalletState::Linked {
                // Already tracked: following it is all that is left to do.
                if self.follow_mode(account.id, entity_id).await? == FollowMode::Off {
                    self.store.set_follow(account.id, entity_id, FollowMode::Cards).await?;
                }
                let mut card = self.entity_view(account, entity_id).await?;
                card.text = format!("Already tracked. You follow it now.\n\n{}", card.text);
                return Ok(card);
            }
            return Ok(Card::text(format!("That wallet is {} for E#{entity_id}. An admin can attach it with <code>/link {} {entity_id}</code>.", existing.state, esc(address))));
        }

        let now = self.now();
        let label = label.trim();
        let label: String = if label.is_empty() { short(address) } else { label.chars().take(40).collect() };
        let entity = self.store.create_entity(&label, account.id, now).await?;
        self.store
            .put_wallet(&Wallet {
                address: address.to_string(),
                entity_id: Some(entity.id),
                state: WalletState::Linked,
                score: 100,
                linked_by: Some(LinkedBy::Root),
                first_seen: now,
                last_signature: None,
            })
            .await?;
        self.store
            .add_evidence(&NewEvidence {
                wallet: address.to_string(),
                entity_id: entity.id,
                kind: "root".into(),
                family: "manual".into(),
                points: 100,
                signatures: Vec::new(),
                details: json!({ "by": account.id }),
                source: "admin".into(),
                observed_at: now,
            })
            .await?;
        self.store.set_follow(account.id, entity.id, FollowMode::Cards).await?;
        self.start_watching(address);
        // Who funded it: the start of its trail, and the entity's funding habit.
        self.enqueue("trace_back", address, Some(entity.id), json!({ "hops": 2 }), 20).await;
        log::info!("{} tracked {address} as {}", account.id, entity.tag());

        let mut card = self.entity_view(account, entity.id).await?;
        card.text = format!("Tracking started. Its recent history is being read now; cards begin with its next trade.\n\n{}", card.text);
        Ok(card)
    }

    async fn cmd_entities(&self, account: &Account) -> StoreResult<Card> {
        let follows = self.store.follows_of_account(account.id).await?;
        let mut rows = Vec::new();
        for entity in self.store.entities().await? {
            if entity.status == EntityStatus::Retired {
                continue;
            }
            let wallets = self.store.wallets_of(entity.id).await?.iter().filter(|w| w.state == WalletState::Linked).count();
            let follow = follows.iter().find(|f| f.entity_id == entity.id).map(|f| f.mode).unwrap_or(FollowMode::Off);
            rows.push((entity, wallets, follow));
        }
        Ok(cards::entities_card(&rows))
    }

    pub(crate) async fn cmd_wallet(&self, address: &str) -> StoreResult<Card> {
        let wallet = self.store.wallet(address).await?;
        let entity = match wallet.as_ref().and_then(|w| w.entity_id) {
            Some(id) => self.store.entity(id).await?,
            None => None,
        };
        let evidence = self.store.evidence_of_wallet(address).await?;
        let trades = self.store.trades_of_wallet(address, 6).await?;
        let transfers = self.store.transfers_of_wallet(address, 8).await?;
        let rows: Vec<TransferRow<'_>> = transfers.iter().map(|t| TransferRow { transfer: t, label: self.labels.get(&t.counterparty) }).collect();
        let label = self.labels.get(address);
        let view = WalletView { address, wallet: wallet.as_ref(), entity: entity.as_ref(), label: label.as_ref(), evidence: &evidence, trades: &trades, transfers: &rows };
        let mut card = cards::wallet_card(&view, self.now());
        if let Some(note) = self.store.doc("casenote", address).await? {
            let reasons: Vec<String> = note.get("reasons").and_then(Value::as_array).map(|list| list.iter().filter_map(Value::as_str).map(|r| format!("• {}", esc(r))).collect()).unwrap_or_default();
            card.text = format!(
                "{}\n\n<b>Reviewer</b> · {} · confidence {:.0}%\n{}",
                card.text,
                esc(note.get("verdict").and_then(Value::as_str).unwrap_or("?")),
                note.get("confidence").and_then(Value::as_f64).unwrap_or(0.0) * 100.0,
                reasons.join("\n")
            );
        }
        Ok(card)
    }

    async fn cmd_follow(&self, account: &Account, args: &[&str]) -> StoreResult<Card> {
        let usage = "Usage: <code>/follow ID off</code>, <code>/follow ID cards</code> or <code>/follow ID auto [SOL]</code>";
        let (Some(id), Some(mode)) = (args.first().and_then(|a| entity_id_of(a)), args.get(1).and_then(|m| FollowMode::parse(&m.to_ascii_lowercase()))) else {
            return Ok(Card::text(usage));
        };
        if self.store.entity(id).await?.is_none() {
            return Ok(Card::text(format!("There is no entity E#{id}.")));
        }
        // An own size for this entity, in SOL; without one the /settings size applies.
        let key = format!("{}:{id}", account.id);
        match args.get(2).map(|size| size.parse::<f64>()) {
            Some(Ok(sol)) if mode == FollowMode::Auto && sol > 0.0 && sol <= 100.0 => self.store.put_doc("follow_size", &key, &json!((sol * 1e9) as u64), self.now()).await?,
            Some(_) => return Ok(Card::text(usage)),
            None => {
                self.store.delete_doc("follow_size", &key).await?;
            }
        }
        self.store.set_follow(account.id, id, mode).await?;
        self.entity_view(account, id).await
    }

    /// The entity, if this account may change it: its creator or an admin.
    async fn editable(&self, account: &Account, id: EntityId) -> StoreResult<Result<Entity, Card>> {
        let Some(entity) = self.store.entity(id).await? else { return Ok(Err(Card::text(format!("There is no entity E#{id}.")))) };
        if !account.is_admin && entity.created_by != account.id {
            return Ok(Err(Card::text("Only the person who added this entity, or an admin, can change it.")));
        }
        Ok(Ok(entity))
    }

    async fn cmd_rename(&self, account: &Account, args: &[&str]) -> StoreResult<Card> {
        let Some(id) = args.first().and_then(|a| entity_id_of(a)) else { return Ok(Card::text("Usage: <code>/rename ID new label</code>")) };
        let label: String = args[1..].join(" ").chars().take(40).collect();
        if label.is_empty() {
            return Ok(Card::text("Usage: <code>/rename ID new label</code>"));
        }
        if let Err(card) = self.editable(account, id).await? {
            return Ok(card);
        }
        self.store.rename_entity(id, &label).await?;
        self.entity_view(account, id).await
    }

    async fn cmd_status(self: &Arc<Self>, account: &Account, args: &[&str], status: EntityStatus) -> StoreResult<Card> {
        let Some(id) = args.first().and_then(|a| entity_id_of(a)) else { return Ok(Card::text("Usage: <code>/retire ID</code> or <code>/revive ID</code>")) };
        if let Err(card) = self.editable(account, id).await? {
            return Ok(card);
        }
        self.store.set_entity_status(id, status).await?;
        self.refresh.notify_one();
        self.entity_view(account, id).await
    }

    async fn cmd_health(&self) -> StoreResult<Card> {
        let counts = self.store.counts().await?;
        let mut card = cards::health_card(&self.health.snapshot(), &counts, &self.rpc_host, self.settings.commitment.as_str(), self.now());
        let (queued, running) = self.store.job_counts().await?;
        let open = self.store.open_positions().await?.len();
        let venue = self.venue().map(|v| v.name()).unwrap_or_else(|| "none".to_string());
        let mode = if self.killed().await {
            "stopped by /kill"
        } else if self.settings.is_live() {
            "LIVE"
        } else {
            "paper only"
        };
        let reviewer = self.reviewer().map(|r| r.model()).unwrap_or_else(|| "off".to_string());
        card.text = format!(
            "{}\nForensics queue: {queued} waiting · {running} running\nTrader: {mode} · venue {} · {} · proofs {}\nReviewer: {}",
            card.text,
            esc(&venue),
            crate::util::count(open, "open position", "open positions"),
            self.proof_count().await,
            esc(&reviewer)
        );
        Ok(card)
    }

    async fn cmd_link(self: &Arc<Self>, account: &Account, args: &[&str]) -> StoreResult<Card> {
        let usage = "Usage: <code>/link ADDRESS ID</code>";
        let (Some(address), Some(id)) = (args.first().filter(|a| is_solana_address(a)), args.get(1).and_then(|a| entity_id_of(a))) else {
            return Ok(Card::text(usage));
        };
        if let Some(label) = self.labels.get(address) {
            return Ok(Card::text(format!("That address is labelled as a service: {} ({}). It cannot be linked to an entity.", esc(&label.name), label.class)));
        }
        let Some(entity) = self.store.entity(id).await? else { return Ok(Card::text(format!("There is no entity E#{id}."))) };
        let now = self.now();
        let existing = self.store.wallet(address).await?;
        if let Some(old) = existing.as_ref().and_then(|w| w.entity_id).filter(|old| *old != id) {
            // It moves: what tied it to the old entity no longer stands.
            self.store.void_evidence(address, old).await?;
        }
        self.store
            .put_wallet(&Wallet { address: address.to_string(), entity_id: Some(id), state: WalletState::Linked, score: 100, linked_by: Some(LinkedBy::Admin), first_seen: now, last_signature: None })
            .await?;
        self.store
            .add_evidence(&NewEvidence {
                wallet: address.to_string(),
                entity_id: id,
                kind: "admin_link".into(),
                family: "manual".into(),
                points: 100,
                signatures: Vec::new(),
                details: json!({ "by": account.id }),
                source: "admin".into(),
                observed_at: now,
            })
            .await?;
        self.start_watching(address);
        log::info!("{} linked {address} to {}", account.id, entity.tag());
        self.entity_view(account, id).await
    }

    async fn cmd_unlink(self: &Arc<Self>, account: &Account, args: &[&str]) -> StoreResult<Card> {
        let Some(address) = args.first().filter(|a| is_solana_address(a)) else { return Ok(Card::text("Usage: <code>/unlink ADDRESS</code>")) };
        let Some(wallet) = self.store.wallet(address).await? else { return Ok(Card::text("That wallet is not known to the bot.")) };
        let Some(id) = wallet.entity_id else { return Ok(Card::text("That wallet is not attached to an entity.")) };
        // Kept as rejected, so it is not proposed for this entity again.
        self.store.void_evidence(address, id).await?;
        self.store.put_wallet(&Wallet { state: WalletState::Rejected, score: 0, linked_by: None, ..wallet }).await?;
        self.refresh.notify_one();
        log::info!("{} unlinked {address} from E#{id}", account.id);
        self.entity_view(account, id).await
    }

    async fn cmd_label(&self, args: &[&str]) -> StoreResult<Card> {
        let classes = LabelClass::all().iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ");
        let usage = format!("Usage: <code>/label ADDRESS class name</code>\nclasses: {classes}");
        let (Some(address), Some(class)) = (args.first().filter(|a| is_solana_address(a)), args.get(1).and_then(|c| LabelClass::parse(&c.to_ascii_lowercase()))) else {
            return Ok(Card::text(usage));
        };
        let name: String = args[2..].join(" ").chars().take(40).collect();
        if name.is_empty() {
            return Ok(Card::text(usage));
        }
        if let Some(wallet) = self.store.wallet(address).await?
            && wallet.state == WalletState::Linked
        {
            return Ok(Card::text("That wallet is linked to an entity. Unlink it before labelling it as a service."));
        }
        let label = Label { address: address.to_string(), class, name: name.to_ascii_lowercase(), source: "admin".into() };
        self.store.put_label(&label).await?;
        self.labels.set(label.clone());
        Ok(Card::text(format!("Labelled {} as {} ({}).", esc(&short(address)), esc(&label.name), label.class)))
    }

    async fn cmd_unlabel(&self, args: &[&str]) -> StoreResult<Card> {
        let Some(address) = args.first().filter(|a| is_solana_address(a)) else { return Ok(Card::text("Usage: <code>/unlabel ADDRESS</code>")) };
        let stored = self.store.delete_label(address).await?;
        let known = self.labels.remove(address);
        Ok(Card::text(if stored || known { "Label removed." } else { "That address has no label." }))
    }

    async fn cmd_code(&self, account: &Account, args: &[&str]) -> StoreResult<Card> {
        let uses = args.first().and_then(|a| a.parse::<i32>().ok()).unwrap_or(1).clamp(1, 50);
        let code = invite_code();
        self.store.create_invite(&code, uses, account.id, self.now()).await?;
        let times = if uses == 1 { "once".to_string() } else { format!("{uses} times") };
        Ok(Card::text(format!("Invite code, good {times}:\n<code>/start {code}</code>")))
    }

    /// A pressed button. `e:ID` opens an entity, `r:ID` refreshes its card,
    /// `f:ID:mode` changes this user's follow.
    async fn on_callback(self: &Arc<Self>, callback: &Value) {
        let Some(callback_id) = callback.get("id").and_then(Value::as_str) else { return };
        let Some(user) = callback.get("from").and_then(|f| f.get("id")).and_then(Value::as_i64) else { return };
        let data = callback.get("data").and_then(Value::as_str).unwrap_or("");
        let message = callback.get("message");
        let chat = message.and_then(|m| m.get("chat")).and_then(|c| c.get("id")).and_then(Value::as_i64);
        let message_id = message.and_then(|m| m.get("message_id")).and_then(Value::as_i64);

        let account = match self.admit(user, "", "").await {
            Ok(Some(account)) => account,
            _ => {
                answer_callback(self.tg.as_ref(), callback_id, "This bot is private.").await;
                return;
            }
        };
        let parts: Vec<&str> = data.split(':').collect();
        let (toast, card, in_place) = match parts.as_slice() {
            // `e` opens an entity as a new message; `r` redraws the card it sits on.
            [kind @ ("e" | "r"), id] => match entity_id_of(id) {
                Some(id) => (String::new(), self.entity_view(&account, id).await.ok(), *kind == "r"),
                None => ("Unknown entity.".to_string(), None, false),
            },
            ["f", id, mode] => match (entity_id_of(id), FollowMode::parse(mode)) {
                (Some(id), Some(mode)) => {
                    let done = self.store.set_follow(account.id, id, mode).await.is_ok();
                    let toast = if !done {
                        "Could not save that."
                    } else if mode == FollowMode::Off {
                        "Muted."
                    } else if mode == FollowMode::Auto {
                        "On auto: copied with your /settings."
                    } else {
                        "Following."
                    };
                    (toast.to_string(), self.entity_view(&account, id).await.ok(), true)
                }
                _ => ("Unknown button.".to_string(), None, false),
            },
            other => self.more_callback(&account, other).await,
        };
        answer_callback(self.tg.as_ref(), callback_id, &toast).await;
        let (Some(card), Some(chat)) = (card, chat) else { return };
        // A follow toggle redraws the entity card it sits on. Opening an
        // entity from a trade card sends a new message, so the trade stays.
        match message_id {
            Some(message_id) if in_place => {
                edit(self.tg.as_ref(), chat, message_id, &card).await;
            }
            _ => {
                send(self.tg.as_ref(), chat, &card).await;
            }
        }
    }
}
