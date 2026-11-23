//! Commands and buttons in a private chat, against fakes.

mod common;

use common::*;
use regnerus::bot::cards::quote_text;
use regnerus::model::*;
use regnerus::store::Store;
use regnerus::util::{ago, esc, fmt_compact, fmt_units, host_of, is_signature, is_solana_address, short};
use serde_json::json;

#[tokio::test(start_paused = true)]
async fn a_stranger_gets_the_private_notice_and_nothing_else() {
    let h = harness();
    for text in ["/start", "/help", &format!("/track {WALLET}"), WALLET, "/start WRONGCODE", "hello"] {
        let replies = h.say(STRANGER, text).await;
        assert_eq!(replies.len(), 1);
        assert!(replies[0].contains("This bot is private"), "{text}: {}", replies[0]);
    }
    assert_eq!(h.store.account(STRANGER).await.unwrap(), None);
    assert_eq!(h.store.counts().await.unwrap().entities, 0);
    assert!(h.rpc.calls.lock().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_admin_gets_help_with_the_admin_section_and_others_without() {
    let h = harness();
    let help = h.say(ADMIN, "/start").await.remove(0);
    assert!(help.contains("/track") && help.contains("<b>Admin</b>") && help.contains("/link"));
    assert!(h.store.account(ADMIN).await.unwrap().unwrap().is_admin);
    assert_eq!(h.store.account(ADMIN).await.unwrap().unwrap().name, "@u1");

    h.store.upsert_account(FRIEND, "friend", false, 0).await.unwrap();
    let help = h.say(FRIEND, "/help@some_bot").await.remove(0);
    assert!(help.contains("/track") && !help.contains("<b>Admin</b>"), "a command addressed to the bot by name still works");
}

#[tokio::test(start_paused = true)]
async fn an_invite_code_admits_exactly_as_many_people_as_it_says() {
    let h = harness();
    let reply = h.say(ADMIN, "/code").await.remove(0);
    let code = reply.split("/start ").nth(1).unwrap().trim_end_matches("</code>").to_string();
    assert_eq!(code.len(), 10, "{reply}");
    assert!(reply.contains("good once"));

    let welcome = h.say(FRIEND, &format!("/start {}", code.to_lowercase())).await.remove(0);
    assert!(welcome.contains("Regnerus"), "codes are not case sensitive: {welcome}");
    assert!(!h.store.account(FRIEND).await.unwrap().unwrap().is_admin);
    assert!(h.say(STRANGER, &format!("/start {code}")).await[0].contains("This bot is private"), "a single-use code is spent");

    // Two codes are never the same.
    let other = h.say(ADMIN, "/code 3").await.remove(0);
    assert!(other.contains("good 3 times") && !other.contains(&code));
    // Someone already in does not need a code again.
    assert!(h.say(FRIEND, "/start").await[0].contains("/track"));
}

#[tokio::test(start_paused = true)]
async fn pasting_an_address_creates_an_entity_with_a_root_wallet() {
    let h = harness();
    let reply = h.say(ADMIN, WALLET).await.remove(0);
    for part in ["Tracking started", "<b>E#1 FwyS…Mcb4</b> · tracking", "You follow it", "<b>Wallets (1)</b>", "linked (root)", "None seen yet."] {
        assert!(reply.contains(part), "missing {part:?} in:\n{reply}");
    }
    let entity = h.store.entity(1).await.unwrap().unwrap();
    assert_eq!((entity.label.as_str(), entity.created_by, entity.status), ("FwyS…Mcb4", ADMIN, EntityStatus::Tracking));
    let wallet = h.store.wallet(WALLET).await.unwrap().unwrap();
    assert_eq!((wallet.entity_id, wallet.state, wallet.linked_by, wallet.score), (Some(1), WalletState::Linked, Some(LinkedBy::Root), 100));
    let evidence = h.store.evidence_of_wallet(WALLET).await.unwrap();
    assert_eq!((evidence.len(), evidence[0].kind.as_str(), evidence[0].details.clone()), (1, "root", json!({ "by": ADMIN })));
    assert_eq!(h.store.follows_of_account(ADMIN).await.unwrap()[0].mode, FollowMode::Cards);
    assert!(h.rpc.calls.lock().iter().any(|c| c.starts_with(&format!("sigs:{WALLET}"))), "its history is read in the background");
    assert_eq!(h.tg.last_buttons(ADMIN), vec![("Mute".to_string(), "f:1:off".to_string()), ("Auto-copy".to_string(), "f:1:auto".to_string()), ("Refresh".to_string(), "r:1".to_string())]);
}

#[tokio::test(start_paused = true)]
async fn tracking_the_same_wallet_again_follows_the_existing_entity() {
    let h = harness();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.store.upsert_account(FRIEND, "friend", false, 0).await.unwrap();
    let reply = h.say(FRIEND, &format!("/track {WALLET} my name for it")).await.remove(0);
    assert!(reply.contains("Already tracked. You follow it now.") && reply.contains("E#1 whale"), "{reply}");
    assert_eq!(h.store.counts().await.unwrap().entities, 1, "one graph for the circle: no second entity");
    assert_eq!(h.store.follows_of_account(FRIEND).await.unwrap()[0].mode, FollowMode::Cards);
}

#[tokio::test(start_paused = true)]
async fn bad_input_gets_a_usage_line_not_an_error() {
    let h = harness();
    for (text, expect) in [
        ("/track", "Usage"),
        ("/track notanaddress", "not a Solana address"),
        ("/entity", "Usage"),
        ("/entity abc", "Usage"),
        ("/entity 99", "There is no entity E#99"),
        ("/wallet nope", "Usage"),
        ("/follow 1", "Usage"),
        ("/follow 99 cards", "There is no entity E#99"),
        ("/rename 1", "Usage"),
        ("/retire", "Usage"),
        ("/link", "Usage"),
        ("/link nope 1", "Usage"),
        ("/label", "Usage"),
        ("/unlink", "Usage"),
        ("/nonsense", "do not know that command"),
        ("gm", "Paste a Solana wallet address"),
    ] {
        let reply = h.say(ADMIN, text).await.remove(0);
        assert!(reply.contains(expect), "{text}: {reply}");
    }
    assert_eq!(h.store.counts().await.unwrap().entities, 0);
}

#[tokio::test(start_paused = true)]
async fn a_label_typed_by_a_user_is_escaped_and_trimmed() {
    let h = harness();
    let long = "x".repeat(80);
    let reply = h.say(ADMIN, &format!("/track {WALLET} <b>bold</b> & {long}")).await.remove(0);
    assert!(reply.contains("&lt;b&gt;bold&lt;/b&gt; &amp;"), "{reply}");
    assert!(!reply.contains("<b>bold</b>"));
    assert_eq!(h.store.entity(1).await.unwrap().unwrap().label.chars().count(), 40);
}

#[tokio::test(start_paused = true)]
async fn admin_commands_are_refused_to_everyone_else() {
    let h = harness();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.store.upsert_account(FRIEND, "friend", false, 0).await.unwrap();
    for text in [format!("/link {WALLET_2} 1"), format!("/unlink {WALLET}"), format!("/label {OTHER} cex_hot binance"), format!("/unlabel {OTHER}"), "/labels".into(), "/code".into(), "/users".into()]
    {
        assert!(h.say(FRIEND, &text).await[0].contains("for admins"), "{text}");
    }
    assert_eq!(h.store.wallets_of(1).await.unwrap().len(), 1);
    assert!(h.store.labels().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn link_and_unlink_move_a_wallet_and_its_evidence() {
    let h = harness();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.say(ADMIN, &format!("/track {OTHER} cabal")).await;

    let reply = h.say(ADMIN, &format!("/link {WALLET_2} E#1")).await.remove(0);
    assert!(reply.contains("<b>Wallets (2)</b>") && reply.contains("linked (admin)"), "{reply}");
    assert_eq!(h.store.evidence_of_wallet(WALLET_2).await.unwrap()[0].kind, "admin_link");

    // Moving it to another entity voids what tied it to the first.
    h.say(ADMIN, &format!("/link {WALLET_2} 2")).await;
    let evidence = h.store.evidence_of_wallet(WALLET_2).await.unwrap();
    assert_eq!(evidence.iter().map(|e| (e.entity_id, e.voided)).collect::<Vec<_>>(), vec![(2, false), (1, true)]);
    assert_eq!(h.store.wallet(WALLET_2).await.unwrap().unwrap().entity_id, Some(2));

    // Unlinking keeps the row as rejected and voids the rest.
    let reply = h.say(ADMIN, &format!("/unlink {WALLET_2}")).await.remove(0);
    assert!(reply.contains("rejected"), "{reply}");
    let wallet = h.store.wallet(WALLET_2).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.entity_id, wallet.linked_by, wallet.score), (WalletState::Rejected, Some(2), None, 0));
    assert!(h.store.evidence_of_wallet(WALLET_2).await.unwrap().iter().all(|e| e.voided));
    assert!(h.say(ADMIN, &format!("/track {WALLET_2}")).await[0].contains("is rejected for E#2"), "a rejected wallet is not silently re-tracked");
    assert!(h.say(ADMIN, &format!("/unlink {MEME_MINT}")).await[0].contains("not known"));
    assert!(h.say(ADMIN, &format!("/link {WALLET_2} 77")).await[0].contains("There is no entity E#77"));
}

#[test]
fn a_card_too_long_for_telegram_is_cut_at_a_line_end() {
    use regnerus::bot::telegram::fit;
    assert_eq!(fit("short"), "short");
    let long: String = (0..400).map(|i| format!("<b>line {i}</b> with some text to fill it out\n")).collect();
    let cut = fit(&long);
    assert!(cut.chars().count() <= 4_096);
    assert!(cut.ends_with("(cut short: too long for one message)"));
    let body = cut.rsplit_once('\n').unwrap().0;
    assert!(body.lines().all(|line| line.starts_with("<b>") && line.ends_with("fill it out")), "no line, and so no tag, is cut in half");
}

#[tokio::test(start_paused = true)]
async fn follow_mute_and_the_buttons() {
    let h = harness();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;

    assert!(h.say(ADMIN, "/follow 1 off").await[0].contains("You are not following it."));
    assert_eq!(h.tg.last_buttons(ADMIN)[0], ("Follow".to_string(), "f:1:cards".to_string()));
    // Auto, with and without a size of its own for this entity.
    assert!(h.say(ADMIN, "/follow 1 auto 0.2").await[0].contains("You follow it on auto"));
    assert_eq!(h.store.follows_of_account(ADMIN).await.unwrap()[0].mode, FollowMode::Auto);
    assert_eq!(h.store.doc("follow_size", "1:1").await.unwrap(), Some(json!(200_000_000u64)));
    assert_eq!(h.tg.last_buttons(ADMIN)[1], ("Cards only".to_string(), "f:1:cards".to_string()));
    assert!(h.say(ADMIN, "/follow 1 cards 0.2").await[0].contains("Usage"), "a size only goes with auto");
    h.say(ADMIN, "/follow 1 auto").await;
    assert_eq!(h.store.doc("follow_size", "1:1").await.unwrap(), None, "without a size the /settings size applies again");
    h.say(ADMIN, "/follow 1 off").await;

    // The Follow button flips it and redraws the card it sits on.
    h.tg.clear();
    h.press(ADMIN, "f:1:cards", 42).await;
    assert_eq!(h.tg.toasts(), vec!["Following."]);
    let edits = h.tg.edits();
    assert_eq!((edits.len(), edits[0].0, edits[0].1), (1, ADMIN, 42));
    assert!(edits[0].2.contains("You follow it"));
    assert!(h.tg.sent().is_empty());

    // Refresh redraws in place; opening an entity from a trade card sends a new message.
    h.tg.clear();
    h.press(ADMIN, "r:1", 42).await;
    assert_eq!((h.tg.edits().len(), h.tg.sent().len()), (1, 0));
    h.tg.clear();
    h.press(ADMIN, "e:1", 99).await;
    assert_eq!((h.tg.edits().len(), h.tg.sent().len()), (0, 1), "the trade card it was pressed on stays as it is");

    // Junk and strangers.
    h.tg.clear();
    h.press(ADMIN, "f:1:auto", 42).await;
    h.press(ADMIN, "zzz", 42).await;
    h.press(STRANGER, "f:1:off", 42).await;
    assert_eq!(h.tg.toasts(), vec!["On auto: copied with your /settings.", "Unknown button.", "This bot is private."]);
    assert_eq!(h.store.follows_of_account(ADMIN).await.unwrap()[0].mode, FollowMode::Auto);
    assert!(h.store.follows_of_account(STRANGER).await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn only_the_creator_or_an_admin_can_rename_or_retire() {
    let h = harness();
    h.store.upsert_account(FRIEND, "friend", false, 0).await.unwrap();
    h.store.upsert_account(STRANGER, "other", false, 0).await.unwrap();
    h.say(FRIEND, &format!("/track {WALLET} mine")).await;

    assert!(h.say(STRANGER, "/rename 1 theirs").await[0].contains("Only the person who added"));
    assert!(h.say(STRANGER, "/retire 1").await[0].contains("Only the person who added"));
    assert!(h.say(FRIEND, "/rename 1 better name").await[0].contains("E#1 better name"));
    assert!(h.say(ADMIN, "/rename E#1 admin name").await[0].contains("E#1 admin name"));

    assert!(h.say(FRIEND, "/retire 1").await[0].contains("· retired"));
    assert!(h.store.watched_wallets().await.unwrap().is_empty());
    assert!(h.say(FRIEND, "/entities").await[0].contains("No entities yet"), "retired entities leave the list");
    assert!(h.say(FRIEND, "/revive 1").await[0].contains("· tracking"));
    assert_eq!(h.store.watched_wallets().await.unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn entities_lists_each_with_wallet_count_and_follow_state() {
    let h = harness();
    h.store.upsert_account(FRIEND, "friend", false, 0).await.unwrap();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.say(ADMIN, &format!("/link {WALLET_2} 1")).await;
    h.say(FRIEND, &format!("/track {OTHER} cabal")).await;
    let list = h.say(ADMIN, "/entities").await.remove(0);
    assert!(list.contains("<b>E#1 whale</b> · 2 wallets · following"), "{list}");
    assert!(list.contains("<b>E#2 cabal</b> · 1 wallet · muted"), "{list}");
    assert_eq!(h.tg.last_buttons(ADMIN), vec![("E#1".to_string(), "e:1".to_string()), ("E#2".to_string(), "e:2".to_string())]);
}

#[tokio::test(start_paused = true)]
async fn labels_mark_services_and_services_cannot_be_tracked_or_linked() {
    let h = harness();
    let reply = h.say(ADMIN, &format!("/label {OTHER} CEX_HOT Binance 2")).await.remove(0);
    assert!(reply.contains("as binance 2 (cex_hot)"), "{reply}");
    assert_eq!(h.store.labels().await.unwrap()[0].name, "binance 2");
    assert!(h.say(ADMIN, "/labels").await[0].contains("cex_hot · binance 2"));
    assert!(h.say(ADMIN, "/labels").await[0].contains("tip · jito"), "the seed is listed too");

    assert!(h.say(ADMIN, &format!("/track {OTHER}")).await[0].contains("labelled as a service: binance 2 (cex_hot)"));
    assert!(h.say(ADMIN, &format!("/track {JITO_TIP}")).await[0].contains("labelled as a service: jito (tip)"));
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    assert!(h.say(ADMIN, &format!("/link {OTHER} 1")).await[0].contains("cannot be linked"));
    assert!(h.say(ADMIN, &format!("/label {WALLET} cex_hot oops")).await[0].contains("Unlink it before labelling"));
    assert!(h.say(ADMIN, &format!("/label {OTHER} nonsense x")).await[0].contains("classes:"));

    assert!(h.say(ADMIN, &format!("/unlabel {OTHER}")).await[0].contains("Label removed."));
    assert!(h.say(ADMIN, &format!("/unlabel {OTHER}")).await[0].contains("has no label"));
    assert!(h.say(ADMIN, &format!("/track {OTHER} now fine")).await[0].contains("Tracking started"));
}

#[tokio::test(start_paused = true)]
async fn stored_labels_are_loaded_over_the_seed_at_start() {
    let h = harness();
    h.store.put_label(&Label { address: OTHER.into(), class: LabelClass::Bridge, name: "debridge".into(), source: "admin".into() }).await.unwrap();
    assert_eq!(h.app.load_labels().await, 1);
    assert_eq!(h.app.labels.get(OTHER).unwrap().class, LabelClass::Bridge);
    assert!(h.app.labels.get(JITO_TIP).is_some());
}

#[tokio::test(start_paused = true)]
async fn the_wallet_case_file_shows_evidence_trades_and_labelled_transfers() {
    let h = harness();
    assert!(h.say(ADMIN, &format!("/wallet {WALLET}")).await[0].contains("Not known to the bot"));
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.say(ADMIN, &format!("/label {OTHER} cex_deposit binance")).await;
    let buy = h.rpc.put(&fixture("pump_buy"));
    h.app.process_signature(WALLET, &buy, true, false).await;
    let out = h.rpc.put(&fixture("sol_transfer"));
    h.app.process_signature(WALLET, &out, true, false).await;
    h.advance(600);

    let card = h.say(ADMIN, &format!("/wallet {WALLET}")).await.remove(0);
    for part in [
        &format!("<code>{WALLET}</code>") as &str,
        "linked of <b>E#1 whale</b> by root · score 100",
        "<b>Evidence</b>",
        "+100 root · manual · admin",
        "<b>Recent trades</b>",
        "0.101 SOL → 3.5M",
        "<b>Recent transfers</b>",
        "0.5 SOL out to",
        "(binance, cex_deposit)",
    ] {
        assert!(card.contains(part), "missing {part:?} in:\n{card}");
    }
    assert_eq!(h.tg.last_buttons(ADMIN), vec![("E#1".to_string(), "e:1".to_string())]);
}

#[tokio::test(start_paused = true)]
async fn pause_stops_cards_and_resume_brings_them_back() {
    let h = harness();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    assert!(h.say(ADMIN, "/pause").await[0].contains("Paused"));
    let first = h.rpc.put(&fixture("pump_buy"));
    h.app.process_signature(WALLET, &first, true, false).await;
    assert_eq!(h.tg.sent_to(ADMIN).iter().filter(|t| t.contains("BUY")).count(), 0);
    assert!(h.say(ADMIN, "/resume").await[0].contains("Resumed"));
    h.advance(120);
    let second = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigAfterResume", T0 + 120));
    h.app.process_signature(WALLET, &second, true, false).await;
    assert_eq!(h.tg.sent_to(ADMIN).iter().filter(|t| t.contains("BUY")).count(), 1);
}

#[tokio::test(start_paused = true)]
async fn health_and_budget_report_real_numbers_and_never_the_rpc_key() {
    let h = harness();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    let sig = h.rpc.put(&fixture("pump_buy"));
    h.app.process_signature(WALLET, &sig, true, false).await;
    h.app.budget.acquire(regnerus::obs::budget::Lane::Forensics, 12).await.unwrap();

    let health = h.say(ADMIN, "/health").await.remove(0);
    for part in ["Stream: DOWN", "RPC: rpc.example · finalized", "1 trade", "1 card", "Block to seen: median 2s", "Stored: 1 entity · 1 wallet · 1 trade · 0 transfers"] {
        assert!(health.contains(part), "missing {part:?} in:\n{health}");
    }
    assert!(!health.contains("/key"), "the RPC URL's path holds the API key");

    let budget = h.say(ADMIN, "/budget").await.remove(0);
    for part in ["12 of 1000000 credits", "• forensics: 12 of 300000", "• copy: 0", "Rate: 60000 of 60000 requests a minute"] {
        assert!(budget.contains(part), "missing {part:?} in:\n{budget}");
    }
    let users = h.say(ADMIN, "/users").await.remove(0);
    assert!(users.contains("<code>1</code> @u1 · admin"), "{users}");
}

#[tokio::test(start_paused = true)]
async fn group_chats_and_non_text_updates_are_ignored() {
    let h = harness();
    h.app.on_update(&json!({ "update_id": 1, "message": { "message_id": 1, "from": { "id": ADMIN }, "chat": { "id": -100, "type": "supergroup" }, "text": format!("/track {WALLET}") } })).await;
    h.app.on_update(&json!({ "update_id": 2, "message": { "message_id": 2, "from": { "id": ADMIN }, "chat": { "id": ADMIN, "type": "private" }, "photo": [] } })).await;
    h.app.on_update(&json!({ "update_id": 3, "edited_message": {} })).await;
    h.app.on_update(&json!({ "update_id": 4, "callback_query": {} })).await;
    settle().await;
    assert!(h.tg.calls.lock().is_empty());
    assert_eq!(h.store.counts().await.unwrap().entities, 0);
}

#[tokio::test(start_paused = true)]
async fn someone_removed_from_admin_ids_keeps_access_but_loses_admin() {
    let h = harness();
    h.store.upsert_account(FRIEND, "was admin", true, 0).await.unwrap();
    assert!(h.say(FRIEND, "/users").await[0].contains("for admins"));
    assert!(!h.store.account(FRIEND).await.unwrap().unwrap().is_admin);
}

// ---- formatting helpers ----

#[test]
fn numbers_read_the_way_a_person_writes_them() {
    assert_eq!(fmt_units(101_000_000, 9, 4), "0.101");
    assert_eq!(fmt_units(1_000_000_000, 9, 4), "1");
    assert_eq!(fmt_units(1_234_567_890_123, 9, 4), "1,234.5678");
    assert_eq!(fmt_units(5, 9, 4), "0", "below the shown precision");
    assert_eq!(fmt_units(250_500_000, 6, 2), "250.5");
    assert_eq!(fmt_units(42, 0, 4), "42");
    assert_eq!(fmt_units(u64::MAX, 9, 4), "18,446,744,073.7095");
    assert_eq!(fmt_compact(3_500_000_000_000, 6), "3.5M");
    assert_eq!(fmt_compact(1_000_000_000_000_000, 9), "1M");
    assert_eq!(fmt_compact(12_340_000, 6), "12.3");
    assert_eq!(fmt_compact(999, 0), "999");
    assert_eq!(fmt_compact(1_500, 0), "1.5K");
    assert_eq!(fmt_compact(2_000_000_000_000_000, 6), "2B");
    assert_eq!(quote_text(Quote::Sol, 101_000_000), "0.101 SOL");
    assert_eq!(quote_text(Quote::Usdc, 250_500_000), "$250.5");
}

#[test]
fn small_text_helpers() {
    assert_eq!(short(WALLET), "FwyS…Mcb4");
    assert_eq!(short("short"), "short");
    assert_eq!(esc("<a & b>"), "&lt;a &amp; b&gt;");
    assert_eq!((ago(5), ago(89), ago(90), ago(5_400), ago(200_000), ago(-3)), ("5s".into(), "89s".into(), "1m".into(), "1h".into(), "2d".into(), "0s".into()));
    assert!(is_solana_address(WALLET) && is_solana_address("11111111111111111111111111111111"));
    for bad in ["", "0x52908400098527886E0F7030069857D2E4169EE7", "FwySUYJVsjuaRpPTEgqmDXnSQgTNFEn", "FwySUYJVsjuaRpPTEgqmDXnSQgTNFEnQxcxF42oaMcbO", "ünïcödé-not-base58-at-all-but-long-enough"] {
        assert!(!is_solana_address(bad), "{bad}");
    }
    assert!(!is_signature(WALLET));
    assert!(is_signature("5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW"));
    assert_eq!(host_of("https://mainnet.helius-rpc.com/?api-key=SECRET"), "mainnet.helius-rpc.com");
    assert_eq!(host_of("wss://user:pass@rpc.example:8900/v2/SECRET"), "rpc.example:8900");
}

#[test]
fn settings_have_safe_defaults_and_say_what_is_missing() {
    use regnerus::chain::solana::rpc::Commitment;
    use regnerus::config::Settings;
    let empty = Settings::from_lookup(&|_| None);
    assert_eq!(empty.problems().len(), 3, "token, RPC and admins: {:?}", empty.problems());
    assert_eq!((empty.db_schema.as_str(), empty.commitment, empty.signal_window_s, empty.backfill_interval_s), ("regnerus", Commitment::Finalized, 30, 300));

    let get = |name: &str| match name {
        "TELEGRAM_BOT_TOKEN" => Some(" 123:abc ".to_string()),
        "ADMIN_IDS" => Some("11, 22;33 junk".to_string()),
        "SOLANA_RPC_URL" => Some("https://rpc.example/v2/KEY".to_string()),
        "SOLANA_COMMITMENT" => Some("Confirmed".to_string()),
        "BACKFILL_INTERVAL_S" => Some("5".to_string()),
        "RPC_FORENSICS_SHARE" => Some("7".to_string()),
        _ => None,
    };
    let set = Settings::from_lookup(&get);
    assert!(set.problems().is_empty());
    assert_eq!(set.telegram_bot_token, "123:abc");
    assert_eq!(set.admin_ids.iter().copied().collect::<Vec<_>>(), vec![11, 22, 33]);
    assert_eq!(set.solana_ws_url, "wss://rpc.example/v2/KEY", "derived from the http URL");
    assert_eq!(set.commitment, Commitment::Confirmed);
    assert_eq!(set.backfill_interval_s, 30, "never more often than every 30 s");
    assert_eq!(set.forensics_share, 1.0, "clamped");
}
