//! Commands and buttons for the trail, the clusters, the trader and the
//! reviewer. `commands.rs` hands over anything it does not know.

use std::sync::Arc;

use serde_json::json;

use crate::app::App;
use crate::bot::cards;
use crate::bot::telegram::Card;
use crate::chain::solana::query::balance;
use crate::model::*;
use crate::obs::budget::Lane;
use crate::store::StoreResult;
use crate::trade::keys::valid_reference;
use crate::trade::sign::Keypair;
use crate::util::{esc, fmt_units, is_solana_address};

fn entity_id_of(text: &str) -> Option<EntityId> {
    text.trim_start_matches(['E', 'e']).trim_start_matches('#').parse().ok().filter(|id| *id > 0)
}

fn lamports_of(text: &str) -> Option<u64> {
    text.parse::<f64>().ok().filter(|sol| sol.is_finite() && *sol >= 0.0 && *sol <= 10_000.0).map(|sol| (sol * 1e9).round() as u64)
}

impl App {
    pub(crate) async fn more_command(self: &Arc<Self>, account: &Account, command: &str, args: &[&str]) -> StoreResult<Card> {
        match command {
            "/trail" => self.cmd_trail(account, args).await,
            "/map" => self.cmd_map(account, args).await,
            "/candidates" => {
                let mut rows = Vec::new();
                for wallet in self.store.wallets_in_state(WalletState::Candidate).await? {
                    let entity = match wallet.entity_id {
                        Some(id) => self.store.entity(id).await?,
                        None => None,
                    };
                    rows.push((wallet, entity));
                }
                rows.sort_by_key(|(wallet, _)| std::cmp::Reverse(wallet.score));
                Ok(cards::candidates_card(&rows))
            }
            "/confirm" => match args.first().filter(|a| is_solana_address(a)) {
                Some(address) => Ok(Card::text(self.do_confirm(account, address).await?)),
                None => Ok(Card::text("Usage: <code>/confirm ADDRESS</code>")),
            },
            "/review" => match args.first().filter(|a| is_solana_address(a)) {
                Some(address) => Ok(Card::text(self.do_review(address).await?)),
                None => Ok(Card::text("Usage: <code>/review ADDRESS</code>")),
            },
            "/merge" => self.cmd_merge(args).await,
            "/split" => match args.first().filter(|a| is_solana_address(a)) {
                Some(address) => match self.split_wallet(address, account.id).await? {
                    Some(entity) => self.entity_view(account, entity.id).await,
                    None => Ok(Card::text("That wallet is not attached to an entity.")),
                },
                None => Ok(Card::text("Usage: <code>/split ADDRESS</code>")),
            },
            "/weights" => {
                if args.first() == Some(&"apply") {
                    if !account.is_admin {
                        return Ok(Card::text("Only an admin can change the points."));
                    }
                    let changed = self.apply_weights().await?;
                    if changed.is_empty() {
                        return Ok(Card::text("Nothing to change: no signal has enough settled cases, or the measured points equal the current ones."));
                    }
                    let lines: Vec<String> = changed.iter().map(|(kind, old, new)| format!("• {kind}: {old} → {new}")).collect();
                    return Ok(Card::text(format!("<b>Points changed</b>\n{}\nCandidates were judged again with the new points.", lines.join("\n"))));
                }
                Ok(cards::weights_card(&self.calibration().await?, account.is_admin))
            }
            "/settings" | "/risk" => {
                let settings = self.trade_settings(account.id).await;
                Ok(cards::settings_card(&settings, self.settings.is_live(), self.trading_wallet(account.id).await.as_ref()))
            }
            "/set" => self.cmd_set(account, args).await,
            "/live" => {
                let mut settings = self.trade_settings(account.id).await;
                settings.live = match args.first().map(|a| a.to_ascii_lowercase()).as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    _ => return Ok(Card::text("Usage: <code>/live on</code> or <code>/live off</code>")),
                };
                self.save_settings(account.id, &settings).await?;
                Ok(Card::text(match (settings.live, self.settings.is_live(), self.trading_wallet(account.id).await.is_some()) {
                    (false, _, _) => "Paper mode: your copies are simulated at the quoted price.".to_string(),
                    (true, false, _) => "Saved, but the server's live switch is off, so your copies stay on paper. An admin turns it on with EXECUTION_LIVE_MODE and EXECUTION_LIVE_ACK.".to_string(),
                    (true, true, false) => "Live is on, but you have no trading wallet yet. See /wallets.".to_string(),
                    (true, true, true) => "LIVE: copies on auto now send real orders from your trading wallet.".to_string(),
                }))
            }
            "/wallets" => self.cmd_wallets(account).await,
            "/newwallet" => self.cmd_new_wallet(account).await,
            "/usewallet" => self.cmd_use_wallet(account, args).await,
            "/positions" => Ok(cards::positions_card(&self.store.positions_of_account(account.id, 0).await?, self.now())),
            "/pnl" => Ok(cards::pnl_card(&self.store.positions_of_account(account.id, 0).await?)),
            "/paper" => {
                let mut rows = Vec::new();
                for entity in self.store.entities().await?.into_iter().filter(|e| e.status != EntityStatus::Retired) {
                    let standing = self.copyability_of(&entity).await?;
                    rows.push((entity, standing));
                }
                Ok(cards::paper_card(&rows))
            }
            "/close" => match args.first().and_then(|a| a.trim_start_matches('#').parse::<i64>().ok()) {
                Some(id) => Ok(Card::text(self.do_close(account, id).await?)),
                None => Ok(Card::text("Usage: <code>/close ID</code> (the number from /positions)")),
            },
            "/kill" => {
                self.store.set_state("kill", "1").await?;
                log::warn!("{} stopped all new copies", account.id);
                Ok(Card::text("Stopped. No new copies are opened, on paper or live. Open positions still follow their exit rules. /unkill restarts."))
            }
            "/unkill" => {
                self.store.set_state("kill", "0").await?;
                Ok(Card::text("Copying is on again."))
            }
            "/copyable" => self.cmd_copyable(args).await,
            _ => Ok(Card::text("I do not know that command. Send /help.")),
        }
    }

    async fn cmd_trail(self: &Arc<Self>, account: &Account, args: &[&str]) -> StoreResult<Card> {
        let Some(address) = args.first().filter(|a| is_solana_address(a)) else { return Ok(Card::text("Usage: <code>/trail ADDRESS [days] [min SOL]</code>")) };
        if let Some(label) = self.labels.get(address) {
            return Ok(Card::text(format!("That address is labelled as a service: {} ({}). A walk from it would reach everyone.", esc(&label.name), label.class)));
        }
        let days = args.get(1).and_then(|d| d.parse::<i64>().ok()).filter(|d| (1..=365).contains(d)).unwrap_or(self.settings.walk_days);
        let min_lamports = args.get(2).and_then(|m| lamports_of(m)).unwrap_or(self.settings.walk_min_lamports);
        self.enqueue("walk", address, None, json!({ "days": days, "min_lamports": min_lamports, "chat": account.id }), 0).await;
        Ok(Card::text(format!(
            "Walking the trail of <code>{}</code>: {days} days, flows of {} SOL and up, at most {} hops, {} wallets and {} credits. The result arrives here.",
            esc(address),
            fmt_units(min_lamports, 9, 2),
            self.settings.walk_hops,
            self.settings.walk_wallets,
            self.settings.walk_credits
        )))
    }

    async fn cmd_map(self: &Arc<Self>, account: &Account, args: &[&str]) -> StoreResult<Card> {
        let Some(mint) = args.first().filter(|a| is_solana_address(a)) else { return Ok(Card::text("Usage: <code>/map MINT</code>")) };
        // Queued like the rest of the forensics, so a slow map never holds up other commands.
        self.enqueue("map", mint, None, json!({ "chat": account.id }), 0).await;
        Ok(Card::text(format!("Building the holder map of <code>{}</code>. It arrives here.", esc(mint))))
    }

    async fn do_confirm(self: &Arc<Self>, account: &Account, address: &str) -> StoreResult<String> {
        if !account.is_admin {
            return Ok("Only an admin can confirm a wallet.".to_string());
        }
        Ok(match self.confirm_wallet(address, account.id).await? {
            Some(change) => format!("Confirmed. {} is linked to E#{}.", crate::util::short(address), change.after.entity_id.unwrap_or_default()),
            None => match self.store.wallet(address).await? {
                Some(wallet) if wallet.state == WalletState::Linked => "It is already linked.".to_string(),
                Some(_) => "Nothing changed.".to_string(),
                None => "That wallet is not known to the bot. Use /link ADDRESS ID.".to_string(),
            },
        })
    }

    async fn do_reject(self: &Arc<Self>, account: &Account, address: &str) -> StoreResult<String> {
        if !account.is_admin {
            return Ok("Only an admin can do that.".to_string());
        }
        Ok(match self.reject_wallet(address).await? {
            Some(entity) => format!("Done. {} is not E#{entity}'s and will not be proposed for it again.", crate::util::short(address)),
            None => "That wallet is not attached to an entity.".to_string(),
        })
    }

    async fn do_review(self: &Arc<Self>, address: &str) -> StoreResult<String> {
        if self.reviewer().is_none() {
            return Ok("No reviewer is configured. Set ANTHROPIC_API_KEY to turn it on.".to_string());
        }
        let Some(wallet) = self.store.wallet(address).await? else { return Ok("That wallet is not known to the bot.".to_string()) };
        let Some(entity) = wallet.entity_id else { return Ok("That wallet is not attached to an entity.".to_string()) };
        self.enqueue("review", address, Some(entity), json!({ "forced": true }), 0).await;
        Ok("Queued for review. The verdict becomes a row of evidence on /wallet.".to_string())
    }

    async fn cmd_merge(self: &Arc<Self>, args: &[&str]) -> StoreResult<Card> {
        let (Some(from), Some(into)) = (args.first().and_then(|a| entity_id_of(a)), args.get(1).and_then(|a| entity_id_of(a))) else {
            return Ok(Card::text("Usage: <code>/merge FROM INTO</code>: everything of entity FROM moves into entity INTO."));
        };
        if from == into || self.store.entity(from).await?.is_none() || self.store.entity(into).await?.is_none() {
            return Ok(Card::text("Give two different entities that exist."));
        }
        let moved = self.merge_entities(from, into).await?;
        Ok(Card::text(format!("Merged. {} moved from E#{from} into E#{into}; E#{from} is retired. Open copies were not touched.", crate::util::count(moved, "wallet", "wallets"))))
    }

    async fn save_settings(&self, account: AccountId, settings: &TradeSettings) -> StoreResult<()> {
        self.store.put_doc("settings", &account.to_string(), &serde_json::to_value(settings).unwrap_or_default(), self.now()).await
    }

    async fn cmd_set(self: &Arc<Self>, account: &Account, args: &[&str]) -> StoreResult<Card> {
        let usage = "Usage: <code>/set KEY VALUE</code>. Keys: size, tp, sl, hold, maxopen, daily, minsource, slippage, entities, window, consensus. /settings shows them all.";
        let (Some(key), Some(value)) = (args.first().map(|k| k.to_ascii_lowercase()), args.get(1).map(|v| v.to_ascii_lowercase())) else { return Ok(Card::text(usage)) };
        let mut settings = self.trade_settings(account.id).await;
        let off = matches!(value.as_str(), "off" | "none" | "0");
        let whole = value.parse::<u32>().ok();
        let ok = match key.as_str() {
            "size" => lamports_of(&value).filter(|l| *l <= 100_000_000_000).map(|l| settings.size_lamports = l).is_some(),
            "tp" => (off || whole.is_some_and(|v| v <= 100_000)).then(|| settings.tp_pct = if off { None } else { whole }).is_some(),
            "sl" => (off || whole.is_some_and(|v| v <= 100)).then(|| settings.sl_pct = if off { None } else { whole }).is_some(),
            "hold" => (off || whole.is_some()).then(|| settings.max_hold_min = if off { None } else { whole }).is_some(),
            "maxopen" => (off || whole.is_some()).then(|| settings.max_open = if off { None } else { whole }).is_some(),
            "daily" => (off || lamports_of(&value).is_some()).then(|| settings.daily_cap_lamports = if off { None } else { lamports_of(&value) }).is_some(),
            "minsource" => lamports_of(&value).map(|l| settings.min_source_lamports = l).is_some(),
            "slippage" => (value == "auto" || value.parse::<u16>().is_ok_and(|b| (1..=5_000).contains(&b))).then(|| settings.slippage_bps = value.parse().ok()).is_some(),
            "entities" => whole.filter(|v| (2..=20).contains(v)).map(|v| settings.min_entities = v).is_some(),
            "window" => whole.filter(|v| (1..=1_440).contains(v)).map(|v| settings.window_min = v).is_some(),
            "consensus" => matches!(value.as_str(), "on" | "off").then(|| settings.require_consensus = value == "on").is_some(),
            _ => false,
        };
        if !ok {
            return Ok(Card::text(usage));
        }
        self.save_settings(account.id, &settings).await?;
        Ok(cards::settings_card(&settings, self.settings.is_live(), self.trading_wallet(account.id).await.as_ref()))
    }

    async fn cmd_wallets(self: &Arc<Self>, account: &Account) -> StoreResult<Card> {
        let provider = self.keys().map(|k| k.name()).unwrap_or("none");
        let Some(wallet) = self.trading_wallet(account.id).await else {
            return Ok(Card::text(format!(
                "You have no trading wallet. It is only needed for live copies; paper copies need none.\n<code>/newwallet</code> makes one and stores its key in the secrets manager.\n<code>/usewallet NAME</code> points at a key you stored there yourself under NAME.\nKeys are held by the secrets provider ({provider}), never in the database."
            )));
        };
        let held = match balance(self.rpc.as_ref(), &wallet.address, Lane::Watch).await {
            Ok(lamports) => format!("{} SOL", fmt_units(lamports, 9, 4)),
            Err(error) => format!("balance unknown ({error})"),
        };
        Ok(Card::text(format!(
            "<b>Trading wallet</b>\n<code>{}</code>\n{held} · key held by {provider} as <code>{}</code>\nFund it only with what you are willing to trade. {} SOL stays in it on buys for fees.",
            esc(&wallet.address),
            esc(&wallet.secret_ref),
            fmt_units(self.settings.sol_reserve_lamports, 9, 3)
        )))
    }

    async fn cmd_new_wallet(self: &Arc<Self>, account: &Account) -> StoreResult<Card> {
        if self.trading_wallet(account.id).await.is_some() {
            return Ok(Card::text("You already have a trading wallet (/wallets). Move its funds out before replacing it with /usewallet."));
        }
        let Some(keys) = self.keys() else { return Ok(Card::text("No key provider is configured.")) };
        if !keys.can_store() {
            return Ok(Card::text(
                "This server's key provider cannot store a new key safely, so the bot will not make one: a key kept only in memory would be lost, with its funds, at the next restart.\nCreate a wallet yourself, put its secret key in the server's secrets under a name like <code>WALLET_ALICE</code>, then send <code>/usewallet WALLET_ALICE</code>.",
            ));
        }
        let key = match Keypair::generate() {
            Ok(key) => key,
            Err(error) => return Ok(Card::text(format!("Could not make a key: {}.", esc(&error.to_string())))),
        };
        let reference = format!("REGNERUS_WALLET_{}", account.id);
        // The key is stored first. Only when that succeeded is the address recorded and shown.
        if let Err(error) = keys.put(&reference, &key.secret_base58()).await {
            return Ok(Card::text(format!("The key could not be stored, so no wallet was made: {}.", esc(&error))));
        }
        let wallet = TradingWallet { address: key.address(), secret_ref: reference };
        self.store.put_doc("twallet", &account.id.to_string(), &serde_json::to_value(&wallet).unwrap_or_default(), self.now()).await?;
        Ok(Card::text(format!(
            "<b>Trading wallet made</b>\n<code>{}</code>\nIts key is in the secrets manager as <code>{}</code>; the bot never shows it. Fund the address with SOL, then <code>/live on</code>.",
            wallet.address, wallet.secret_ref
        )))
    }

    async fn cmd_use_wallet(self: &Arc<Self>, account: &Account, args: &[&str]) -> StoreResult<Card> {
        let Some(reference) = args.first().filter(|r| valid_reference(r)) else {
            return Ok(Card::text("Usage: <code>/usewallet NAME</code>, where NAME is the secret's name: capitals, digits and underscores."));
        };
        // A secret belongs to whoever registered it first.
        for (owner, value) in self.store.docs("twallet", "", 10_000).await? {
            if owner != account.id.to_string() && value.get("secret_ref").and_then(|v| v.as_str()) == Some(reference) {
                return Ok(Card::text("That secret is already someone else's trading wallet."));
            }
        }
        let Some(keys) = self.keys() else { return Ok(Card::text("No key provider is configured.")) };
        let secret = match keys.reveal(reference).await {
            Ok(secret) => secret,
            Err(error) => return Ok(Card::text(format!("Could not read that secret: {}.", esc(&error)))),
        };
        let key = match Keypair::from_secret(&secret) {
            Ok(key) => key,
            Err(error) => return Ok(Card::text(format!("That secret is not a Solana key: {}.", esc(&error.to_string())))),
        };
        let wallet = TradingWallet { address: key.address(), secret_ref: reference.to_string() };
        self.store.put_doc("twallet", &account.id.to_string(), &serde_json::to_value(&wallet).unwrap_or_default(), self.now()).await?;
        Ok(Card::text(format!("Trading wallet set to <code>{}</code>. /wallets shows its balance.", wallet.address)))
    }

    async fn do_close(self: &Arc<Self>, account: &Account, id: i64) -> StoreResult<String> {
        let Some(position) = self.store.position(id).await?.filter(|p| p.account == account.id) else { return Ok("You have no position with that number.".to_string()) };
        if position.status != PositionStatus::Open {
            return Ok(format!("Position #{id} is {}.", position.status));
        }
        let app = self.clone();
        tokio::spawn(async move { app.close_copy(id, 1.0, "manual").await });
        Ok(format!("Closing #{id}."))
    }

    /// The Copy button under a buy card: the user's own call, so the
    /// entity's standing is not checked. The stop switch and the token
    /// screen still are.
    async fn do_copy(self: &Arc<Self>, account: &Account, signal_id: i64) -> StoreResult<String> {
        let Some(signal) = self.store.signal(signal_id).await?.filter(|s| s.side == Side::Buy) else { return Ok("That signal is gone.".to_string()) };
        if self.venue().is_none() {
            return Ok("No swap venue is configured.".to_string());
        }
        if self.killed().await {
            return Ok("Trading is stopped by an admin.".to_string());
        }
        let settings = self.trade_settings(account.id).await;
        if settings.size_lamports == 0 {
            return Ok("Your size is zero. /set size 0.05".to_string());
        }
        let live = settings.live && self.settings.is_live();
        let mode = if live { PositionMode::Live } else { PositionMode::Paper };
        let app = self.clone();
        let account_id = account.id;
        tokio::spawn(async move {
            // A second tap finds the position already there and does nothing.
            app.open_copy(account_id, mode, settings.size_lamports, &signal, settings.slippage_bps, None).await;
        });
        Ok(if live { "Copying, live.".to_string() } else { "Copying, on paper.".to_string() })
    }

    async fn cmd_copyable(self: &Arc<Self>, args: &[&str]) -> StoreResult<Card> {
        let usage = "Usage: <code>/copyable ID on</code>, <code>off</code> or <code>auto</code>. On and off pin the flag; auto hands it back to the paper results and clears a bait flag.";
        let (Some(id), Some(choice)) = (args.first().and_then(|a| entity_id_of(a)), args.get(1).map(|c| c.to_ascii_lowercase())) else { return Ok(Card::text(usage)) };
        let Some(entity) = self.store.entity(id).await? else { return Ok(Card::text(format!("There is no entity E#{id}."))) };
        let key = id.to_string();
        match choice.as_str() {
            "on" | "off" => {
                self.store.set_entity_flags(id, Some(choice == "on"), Some(false)).await?;
                self.store.put_doc("copygate", &key, &json!({ "manual": true, "at": self.now() }), self.now()).await?;
                Ok(Card::text(format!("{} is pinned as {}. The paper results no longer change it.", esc(&entity.tag()), if choice == "on" { "copyable" } else { "not copyable" })))
            }
            "auto" => {
                self.store.set_entity_flags(id, Some(false), Some(false)).await?;
                self.store.put_doc("copygate", &key, &json!({ "fails": 0, "at": self.now() }), self.now()).await?;
                self.copyability_pass().await?;
                let standing = self.store.entity(id).await?.is_some_and(|e| e.copyable);
                Ok(Card::text(format!("{} is back on the paper results: {}.", esc(&entity.tag()), if standing { "copyable" } else { "not yet copyable (/paper says why)" })))
            }
            _ => Ok(Card::text(usage)),
        }
    }

    /// Buttons `commands.rs` does not know. Returns the toast, a card, and
    /// whether the card replaces the message the button sits on.
    pub(crate) async fn more_callback(self: &Arc<Self>, account: &Account, parts: &[&str]) -> (String, Option<Card>, bool) {
        let done = |result: StoreResult<String>| (result.unwrap_or_else(|error| format!("Something went wrong: {error}")), None, false);
        match parts {
            ["c", address] => done(self.do_confirm(account, address).await),
            ["n", address] => done(self.do_reject(account, address).await),
            ["v", address] if account.is_admin => done(self.do_review(address).await),
            ["v", _] => ("Only an admin can ask for a review.".to_string(), None, false),
            ["w", _] => ("It stays a candidate: watched, no cards.".to_string(), None, false),
            ["y", address] => match self.cmd_wallet(address).await {
                Ok(card) => (String::new(), Some(card), false),
                Err(error) => (format!("Something went wrong: {error}"), None, false),
            },
            ["x", id] => match entity_id_of(id) {
                Some(id) => done(self.store.set_entity_status(id, EntityStatus::Retired).await.map(|_| {
                    self.refresh.notify_one();
                    format!("E#{id} is retired.")
                })),
                None => ("Unknown entity.".to_string(), None, false),
            },
            ["k", id] => match entity_id_of(id) {
                Some(id) => done(self.store.put_doc("lost", &id.to_string(), &json!({ "at": self.now() }), self.now()).await.map(|_| "Still watching.".to_string())),
                None => ("Unknown entity.".to_string(), None, false),
            },
            ["z", id] => match id.parse::<i64>() {
                Ok(id) => done(self.do_close(account, id).await),
                Err(_) => ("Unknown position.".to_string(), None, false),
            },
            ["b", id] => match id.parse::<i64>() {
                Ok(id) => done(self.do_copy(account, id).await),
                Err(_) => ("Unknown signal.".to_string(), None, false),
            },
            _ => ("Unknown button.".to_string(), None, false),
        }
    }
}
