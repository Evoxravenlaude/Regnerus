//! From a signature to a stored trade and a card, against fakes.

mod common;

use common::*;
use regnerus::model::*;
use regnerus::pipeline::Outcome;
use regnerus::store::Store;

/// An admin tracks WALLET as entity 1 and follows it.
async fn tracked() -> Harness {
    let h = harness();
    let replies = h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    assert!(replies[0].contains("Tracking started"), "{}", replies[0]);
    h.tg.clear();
    h.rpc.calls.lock().clear();
    h
}

#[tokio::test(start_paused = true)]
async fn a_live_buy_becomes_one_stored_trade_and_one_card() {
    let h = tracked().await;
    let sig = h.rpc.put(&fixture("pump_buy"));
    assert_eq!(h.app.process_signature(WALLET, &sig, true, false).await, Outcome::Trade { new: true });

    let trades = h.store.trades_of_entity(1, 10).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!((trades[0].side, trades[0].quote_amount_raw, trades[0].entity_id, trades[0].seen_at), (Side::Buy, 101_000_000, Some(1), T0 + 2));
    assert!(trades[0].fingerprint.self_paid);

    let cards = h.tg.sent_to(ADMIN);
    assert_eq!(cards.len(), 1);
    let card = &cards[0];
    for part in ["🟢 <b>BUY</b> · <b>E#1 whale</b>", "0.101 SOL → 3.5M", "pump.fun · wallet", "priority 0.00001 SOL", "seen 2s after the block", &format!("<code>{MEME_MINT}</code>")] {
        assert!(card.contains(part), "missing {part:?} in:\n{card}");
    }
    let buttons = h.tg.last_buttons(ADMIN);
    assert_eq!(buttons[0], ("Transaction".to_string(), format!("https://solscan.io/tx/{sig}")));
    assert_eq!(buttons[2], ("E#1".to_string(), "e:1".to_string()));

    assert_eq!(h.rpc.fetches(), vec![format!("tx:{sig}:copy")], "a live event uses the copy lane, once");
    let health = h.app.health.snapshot();
    assert_eq!((health.trades, health.cards, health.tx_fetched, health.delay_median_s), (1, 1, 1, Some(2)));
}

#[tokio::test(start_paused = true)]
async fn the_same_signature_is_handled_once() {
    let h = tracked().await;
    let sig = h.rpc.put(&fixture("pump_buy"));
    h.app.process_signature(WALLET, &sig, true, false).await;
    assert_eq!(h.app.process_signature(WALLET, &sig, true, false).await, Outcome::Other, "the stream delivered it twice");
    assert_eq!(h.tg.sent_to(ADMIN).len(), 1);
    assert_eq!(h.rpc.fetches().len(), 1, "no second lookup");
    assert_eq!(h.store.counts().await.unwrap().trades, 1);
}

#[tokio::test(start_paused = true)]
async fn only_unpaused_followers_get_the_card() {
    let h = tracked().await;
    for (user, name) in [(FRIEND, "friend"), (STRANGER, "muted"), (4, "paused"), (5, "bystander")] {
        h.store.upsert_account(user, name, false, 0).await.unwrap();
    }
    h.store.set_follow(FRIEND, 1, FollowMode::Cards).await.unwrap();
    h.store.set_follow(STRANGER, 1, FollowMode::Off).await.unwrap();
    h.store.set_follow(4, 1, FollowMode::Cards).await.unwrap();
    h.store.set_paused(4, true).await.unwrap();

    let sig = h.rpc.put(&fixture("pump_buy"));
    h.app.process_signature(WALLET, &sig, true, false).await;
    let chats: Vec<i64> = h.tg.sent().iter().map(|(chat, _)| *chat).collect();
    assert_eq!(chats, vec![ADMIN, FRIEND], "muted, paused and non-following accounts get nothing");
}

#[tokio::test(start_paused = true)]
async fn a_blocked_follower_does_not_stop_the_others() {
    let h = tracked().await;
    h.store.upsert_account(FRIEND, "friend", false, 0).await.unwrap();
    h.store.set_follow(FRIEND, 1, FollowMode::Cards).await.unwrap();
    h.tg.blocked.lock().insert(ADMIN);
    let sig = h.rpc.put(&fixture("pump_buy"));
    assert_eq!(h.app.process_signature(WALLET, &sig, true, false).await, Outcome::Trade { new: true });
    assert_eq!(h.store.signal_messages(1).await.unwrap().iter().map(|(a, _)| *a).collect::<Vec<_>>(), vec![FRIEND], "only a delivered card is remembered for later edits");
}

#[tokio::test(start_paused = true)]
async fn a_second_wallet_buying_inside_the_window_edits_the_same_card() {
    let h = tracked().await;
    assert!(h.say(ADMIN, &format!("/link {WALLET_2} 1")).await[0].contains("Wallets (2)"));
    h.tg.clear();

    let first = h.rpc.put(&fixture("pump_buy"));
    h.app.process_signature(WALLET, &first, true, false).await;
    let second = h.rpc.put(&as_wallet("pump_buy", WALLET_2, "sigSecondWallet", T0 + 10));
    h.advance(10);
    assert_eq!(h.app.process_signature(WALLET_2, &second, true, false).await, Outcome::Trade { new: true });

    assert_eq!(h.tg.sent_to(ADMIN).len(), 1, "no second card for the same entity buy");
    let edits = h.tg.edits();
    assert_eq!(edits.len(), 1);
    let (chat, message_id, text) = &edits[0];
    let card_id = h.store.signal_messages(1).await.unwrap()[0].1;
    assert_eq!((*chat, *message_id), (ADMIN, card_id), "the first card is edited in place");
    for part in ["0.202 SOL → 7M", "2 buys by 2 wallets in 10s"] {
        assert!(text.contains(part), "missing {part:?} in:\n{text}");
    }
    let signal = h.store.open_signal(1, MEME_MINT, Side::Buy, 0).await.unwrap().unwrap();
    assert_eq!((signal.trades, signal.wallets.len(), signal.quote_amount_raw, signal.first_at, signal.last_at), (2, 2, 202_000_000, T0, T0 + 10));
}

#[tokio::test(start_paused = true)]
async fn a_buy_after_the_window_is_a_new_card_and_a_sell_is_its_own_signal() {
    let h = tracked().await;
    let first = h.rpc.put(&fixture("pump_buy"));
    h.app.process_signature(WALLET, &first, true, false).await;

    h.advance(60);
    let later = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigLater", T0 + 60));
    h.app.process_signature(WALLET, &later, true, false).await;
    assert_eq!(h.tg.sent_to(ADMIN).len(), 2, "31 seconds or more apart is a new signal");
    assert!(h.tg.edits().is_empty());

    let sell = h.rpc.put(&as_wallet("jup_sell", WALLET, "sigSell", T0 + 65));
    h.advance(5);
    h.app.process_signature(WALLET, &sell, true, false).await;
    let card = h.tg.last_to(ADMIN);
    for part in ["🔴 <b>SELL</b> · <b>E#1 whale</b>", "→ $250.5", "Jupiter · wallet"] {
        assert!(card.contains(part), "missing {part:?} in:\n{card}");
    }
    assert_eq!(h.tg.sent_to(ADMIN).len(), 3);
}

#[tokio::test(start_paused = true)]
async fn transfers_are_stored_without_a_card_and_tips_are_not_transfers() {
    let h = tracked().await;
    let sig = h.rpc.put(&fixture("sol_transfer"));
    assert_eq!(h.app.process_signature(WALLET, &sig, true, false).await, Outcome::Transfers(1));
    let stored = h.store.transfers_of_wallet(WALLET, 10).await.unwrap();
    assert_eq!((stored[0].direction, stored[0].counterparty.as_str(), stored[0].amount_raw, stored[0].mint.clone()), (Direction::Out, OTHER, 500_000_000, None));
    assert!(h.tg.sent().is_empty());

    // The same transfer to a labelled tip account is how a transaction was sent, not money moved.
    let tip = serde_json::to_string(&fixture("sol_transfer")).unwrap().replace(OTHER, JITO_TIP);
    let mut tip: serde_json::Value = serde_json::from_str(&tip).unwrap();
    tip["transaction"]["signatures"][0] = serde_json::json!("sigTipOnly");
    let sig = h.rpc.put(&tip);
    assert_eq!(h.app.process_signature(WALLET, &sig, true, false).await, Outcome::Transfers(0));
    assert_eq!(h.store.counts().await.unwrap().transfers, 1);
}

#[tokio::test(start_paused = true)]
async fn a_transaction_the_node_does_not_have_yet_is_retried_then_left_for_the_backfill() {
    let h = tracked().await;
    let sig = h.rpc.put(&fixture("pump_buy"));
    // Null twice, then there: still one call from the pipeline's side.
    h.rpc.not_yet.lock().insert(sig.clone(), 2);
    assert_eq!(h.app.process_signature(WALLET, &sig, true, false).await, Outcome::Trade { new: true });
    assert_eq!(h.rpc.fetches().len(), 3);

    // Null on every try: reported missing, and not marked as handled.
    let late = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigLate", T0 + 3));
    h.rpc.not_yet.lock().insert(late.clone(), 3);
    assert_eq!(h.app.process_signature(WALLET, &late, true, false).await, Outcome::Missing);
    assert_eq!(h.app.health.snapshot().tx_missing, 1);
    h.rpc.push_sig(WALLET, &late, false);
    assert_eq!(h.app.backfill_wallet(WALLET).await, 1, "the backfill picks it up");
    assert_eq!(h.store.counts().await.unwrap().trades, 2);
}

#[tokio::test(start_paused = true)]
async fn an_rpc_error_is_reported_and_retried_later() {
    let h = tracked().await;
    let sig = h.rpc.put(&fixture("pump_buy"));
    h.rpc.broken.lock().insert(sig.clone());
    assert!(matches!(h.app.process_signature(WALLET, &sig, true, false).await, Outcome::Failed(_)));
    assert_eq!(h.app.health.snapshot().errors, 1);
    h.rpc.broken.lock().clear();
    assert_eq!(h.app.process_signature(WALLET, &sig, true, false).await, Outcome::Trade { new: true });
}

#[tokio::test(start_paused = true)]
async fn tracking_reads_history_silently_then_backfill_fires_cards_oldest_first() {
    let h = harness();
    // History that exists before the wallet is tracked.
    let old = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigOld", T0 - 500));
    h.rpc.push_sig(WALLET, &old, false);
    let replies = h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    assert_eq!(replies.len(), 1, "tracking a wallet never fires cards for its past: {replies:?}");
    assert_eq!(h.store.counts().await.unwrap().trades, 1, "but the history is stored");
    assert_eq!(h.store.wallet(WALLET).await.unwrap().unwrap().last_signature.as_deref(), Some("sigOld"));
    assert!(h.rpc.fetches().iter().all(|c| c.ends_with(":watch")), "history uses the watch lane");
    assert!(h.say(ADMIN, "/entity 1").await[0].contains("0.101 SOL → 3.5M"), "and shows on the entity card");
    h.tg.clear();

    // Three things happen while the stream is down: a buy, a failed transaction, a sell.
    let buy = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigGapBuy", T0 + 1));
    let sell = h.rpc.put(&as_wallet("jup_sell", WALLET, "sigGapSell", T0 + 2));
    h.rpc.push_sig(WALLET, &buy, false);
    h.rpc.push_sig(WALLET, "sigFailed", true);
    h.rpc.push_sig(WALLET, &sell, false);
    h.rpc.calls.lock().clear();
    assert_eq!(h.app.backfill_all().await, 2);
    let cards = h.tg.sent_to(ADMIN);
    assert_eq!(cards.len(), 2);
    assert!(cards[0].contains("BUY") && cards[1].contains("SELL"), "oldest first");
    assert_eq!(h.rpc.fetches(), vec!["tx:sigGapBuy:watch".to_string(), "tx:sigGapSell:watch".to_string()], "the failed one is never fetched");
    assert_eq!(h.store.wallet(WALLET).await.unwrap().unwrap().last_signature.as_deref(), Some("sigGapSell"));

    // Nothing new: one cheap call, no lookups.
    h.rpc.calls.lock().clear();
    assert_eq!(h.app.backfill_all().await, 0);
    assert_eq!(h.rpc.calls.lock().clone(), vec![format!("sigs:{WALLET}:sigGapSell:watch")]);
}

#[tokio::test(start_paused = true)]
async fn the_backfill_does_not_fetch_what_the_stream_already_delivered() {
    let h = tracked().await;
    let sig = h.rpc.put(&fixture("pump_buy"));
    h.app.process_signature(WALLET, &sig, true, false).await;
    h.rpc.push_sig(WALLET, &sig, false);
    h.rpc.calls.lock().clear();
    h.app.backfill_wallet(WALLET).await;
    assert!(h.rpc.fetches().is_empty(), "already handled live: {:?}", h.rpc.fetches());
    assert_eq!(h.tg.sent_to(ADMIN).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_failed_lookup_keeps_the_cursor_back_so_the_next_pass_retries() {
    let h = tracked().await;
    let seed = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigSeed", T0 - 100));
    h.rpc.push_sig(WALLET, &seed, false);
    h.app.backfill_wallet(WALLET).await;
    assert_eq!(h.store.wallet(WALLET).await.unwrap().unwrap().last_signature.as_deref(), Some("sigSeed"));

    let gap = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigGap", T0 + 1));
    h.rpc.push_sig(WALLET, &gap, false);
    h.rpc.broken.lock().insert(gap.clone());
    assert_eq!(h.app.backfill_wallet(WALLET).await, 0);
    assert_eq!(h.store.wallet(WALLET).await.unwrap().unwrap().last_signature.as_deref(), Some("sigSeed"), "the cursor did not move past a lookup that failed");
    h.rpc.broken.lock().clear();
    assert_eq!(h.app.backfill_wallet(WALLET).await, 1);
    assert_eq!(h.store.wallet(WALLET).await.unwrap().unwrap().last_signature.as_deref(), Some("sigGap"));
}

#[tokio::test(start_paused = true)]
async fn with_no_history_wanted_tracking_still_sets_the_cursor() {
    let h = harness_with(&[("HISTORY_ON_TRACK", "0")]);
    let old = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigOld", T0 - 500));
    h.rpc.push_sig(WALLET, &old, false);
    h.say(ADMIN, &format!("/track {WALLET}")).await;
    assert_eq!(h.store.counts().await.unwrap().trades, 0);
    assert!(h.rpc.fetches().is_empty());
    assert_eq!(h.store.wallet(WALLET).await.unwrap().unwrap().last_signature.as_deref(), Some("sigOld"));
}

#[tokio::test(start_paused = true)]
async fn an_old_trade_is_stored_without_a_card() {
    let h = tracked().await;
    h.advance(2 * 3_600);
    let sig = h.rpc.put(&fixture("pump_buy"));
    assert_eq!(h.app.process_signature(WALLET, &sig, false, false).await, Outcome::Trade { new: true });
    assert!(h.tg.sent().is_empty(), "two hours late is not news");
    assert_eq!(h.app.health.snapshot().delay_samples, 0, "and a backfilled trade does not count toward the live delay");
}

#[tokio::test(start_paused = true)]
async fn unknown_wallets_candidates_and_retired_entities_send_nothing() {
    let h = tracked().await;
    let sig = h.rpc.put(&as_wallet("pump_buy", OTHER, "sigUnknown", T0));
    assert_eq!(h.app.process_signature(OTHER, &sig, true, false).await, Outcome::UnknownWallet);
    assert!(h.rpc.fetches().is_empty(), "nothing is fetched for a wallet the bot does not know");

    // A candidate's trade is kept as evidence material but is not yet the entity's.
    h.store.put_wallet(&Wallet { address: WALLET_2.into(), entity_id: Some(1), state: WalletState::Candidate, score: 58, linked_by: None, first_seen: T0, last_signature: None }).await.unwrap();
    let sig = h.rpc.put(&as_wallet("pump_buy", WALLET_2, "sigCandidate", T0));
    assert_eq!(h.app.process_signature(WALLET_2, &sig, true, false).await, Outcome::Trade { new: true });
    assert!(h.tg.sent().is_empty());
    assert_eq!(h.store.trades_of_wallet(WALLET_2, 5).await.unwrap().len(), 1);
    assert!(h.store.trades_of_entity(1, 5).await.unwrap().is_empty());

    h.say(ADMIN, "/retire 1").await;
    h.tg.clear();
    let sig = h.rpc.put(&fixture("pump_buy"));
    assert_eq!(h.app.process_signature(WALLET, &sig, true, false).await, Outcome::Trade { new: true });
    assert!(h.tg.sent().is_empty(), "a retired entity is silent");
}

#[tokio::test(start_paused = true)]
async fn the_stream_source_lists_exactly_the_watched_wallets() {
    use regnerus::chain::solana::stream::{AddressSource, StreamMsg};
    let h = tracked().await;
    assert_eq!(h.app.addresses().await, vec![WALLET.to_string()]);
    h.say(ADMIN, &format!("/link {WALLET_2} 1")).await;
    let mut both = h.app.addresses().await;
    both.sort();
    let mut want = vec![WALLET.to_string(), WALLET_2.to_string()];
    want.sort();
    assert_eq!(both, want);
    h.say(ADMIN, &format!("/unlink {WALLET_2}")).await;
    assert_eq!(h.app.addresses().await, vec![WALLET.to_string()], "a rejected wallet is no longer subscribed");

    // A reconnect message runs a backfill pass over them.
    h.tg.clear();
    let sig = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigWhileDown", T0 + 1));
    h.rpc.push_sig(WALLET, &sig, false);
    h.app.on_stream(StreamMsg::Connected { wallets: 1 }).await;
    assert_eq!(h.tg.sent_to(ADMIN).len(), 1);
    h.app.on_stream(StreamMsg::Tx { wallet: WALLET.into(), signature: sig }).await;
    assert_eq!(h.tg.sent_to(ADMIN).len(), 1, "the stream delivering it afterwards changes nothing");
}

#[tokio::test(start_paused = true)]
async fn credits_reach_the_ledger_once() {
    let h = tracked().await;
    h.app.budget.acquire(regnerus::obs::budget::Lane::Watch, 7).await.unwrap();
    h.app.flush_ledger().await;
    h.app.flush_ledger().await;
    assert_eq!(h.store.credits("2025-10-02").await.unwrap(), vec![("watch".to_string(), 7)]);
}

#[tokio::test(start_paused = true)]
async fn a_fresh_wallets_first_transaction_is_news_not_history() {
    // Tracked before it has ever transacted: the case this bot exists for.
    let h = tracked().await;
    assert_eq!(h.store.wallet(WALLET).await.unwrap().unwrap().last_signature.as_deref(), Some(""), "read once, nothing there yet");

    // Its first buy lands while the stream is down and arrives by backfill.
    let first = h.rpc.put(&as_wallet("pump_buy", WALLET, "sigFirstEver", T0 + 1));
    h.rpc.push_sig(WALLET, &first, false);
    assert_eq!(h.app.backfill_wallet(WALLET).await, 1);
    assert_eq!(h.tg.sent_to(ADMIN).len(), 1, "a card, because it happened after tracking began");
    assert_eq!(h.store.wallet(WALLET).await.unwrap().unwrap().last_signature.as_deref(), Some("sigFirstEver"));
}
