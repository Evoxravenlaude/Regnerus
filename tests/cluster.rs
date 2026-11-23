//! Clusters and consensus: co-buy windows, snipers and crowds, blocs,
//! entity votes, and the holder map.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use regnerus::chain::solana::rpc::SignatureInfo;
use regnerus::cluster::bubble::{Holder, cluster};
use regnerus::consensus::{blocs, tally};
use regnerus::identity::score::kind;
use regnerus::model::*;
use regnerus::obs::budget::Lane;
use regnerus::store::Store;
use serde_json::json;

fn buy_signal(id: i64, entity: EntityId, mint: &str, at: i64, wallets: usize) -> Signal {
    Signal {
        id,
        entity_id: entity,
        mint: mint.into(),
        side: Side::Buy,
        first_at: at,
        last_at: at,
        quote: Quote::Sol,
        quote_amount_raw: 1,
        token_amount_raw: 1,
        decimals: 6,
        wallets: (0..wallets).map(|i| format!("w{i}")).collect(),
        trades: 1,
        venue: Venue::PumpFun,
    }
}

#[test]
fn entities_that_buy_together_most_of_the_time_are_one_bloc() {
    let mut signals = Vec::new();
    // E#1 and E#2 buy the same six tokens within seconds of each other.
    for i in 0..6 {
        signals.push(buy_signal(i, 1, &format!("mint{i}"), 1_000 * i, 1));
        signals.push(buy_signal(100 + i, 2, &format!("mint{i}"), 1_000 * i + 20, 1));
    }
    // E#3 shares one of those tokens, hours apart, and has five buys of its own.
    signals.push(buy_signal(200, 3, "mint0", 50_000, 1));
    for i in 0..5 {
        signals.push(buy_signal(201 + i, 3, &format!("own{i}"), 2_000 * i, 1));
    }
    // E#4 bought together with E#1 twice: too little history to call.
    signals.push(buy_signal(300, 4, "mint0", 5, 1));
    signals.push(buy_signal(301, 4, "mint1", 1_005, 1));
    assert_eq!(blocs(&signals), BTreeMap::from([(1, 1), (2, 1)]));
    assert!(blocs(&[]).is_empty());
}

#[test]
fn consensus_counts_entities_once_per_bloc_and_skips_crowded_buys() {
    let buys = vec![buy_signal(1, 1, "m", 100, 2), buy_signal(2, 1, "m", 130, 1), buy_signal(3, 2, "m", 140, 1), buy_signal(4, 3, "m", 150, 1), buy_signal(5, 4, "m", 10, 1)];
    let no_blocs = BTreeMap::new();
    let none = BTreeSet::new();

    let all = tally(&buys, &no_blocs, &none, None, 50);
    assert_eq!(all.entities, vec![(1, 2), (2, 1), (3, 1)], "two signals of one entity are one vote; the old buy is outside the window");
    assert_eq!(all.independent, 3);
    // E#1 and E#2 are a bloc: three entities, two independent.
    let bloc = BTreeMap::from([(1, 1), (2, 1)]);
    assert_eq!(tally(&buys, &bloc, &none, None, 50).independent, 2);
    // A buy made in a crowd of snipers does not count.
    assert_eq!(tally(&buys, &no_blocs, &BTreeSet::from([3]), None, 50).independent, 2);
    // Only the entities this user follows vote.
    let follows = BTreeSet::from([1, 3]);
    let mine = tally(&buys, &no_blocs, &none, Some(&follows), 50);
    assert_eq!((mine.entities.len(), mine.independent), (2, 2));
}

fn holder(owner: &str, amount: u64) -> Holder {
    Holder { owner: owner.into(), amount, service: None, entity: None }
}

#[test]
fn holders_joined_by_ties_form_clusters_largest_first() {
    let holders = vec![holder("a", 30), holder("b", 20), holder("c", 25), holder("d", 5), holder("e", 10)];
    let ties = vec![
        ("a".to_string(), "b".to_string(), "same funder".to_string()),
        ("b".to_string(), "d".to_string(), "a transfer between them".to_string()),
        ("x".to_string(), "c".to_string(), "not a holder".to_string()),
    ];
    let clusters = cluster(&holders, &ties);
    assert_eq!(clusters.len(), 1, "c and e are on their own; a tie to a non-holder joins nothing");
    assert_eq!((clusters[0].members.clone(), clusters[0].amount), (vec!["a".to_string(), "b".to_string(), "d".to_string()], 55));
    assert_eq!(clusters[0].ties, vec!["same funder".to_string(), "a transfer between them".to_string()]);
    assert!(cluster(&holders, &[]).is_empty());
}

/// Lists a signature in an address's history with its slot and time.
fn list(h: &Harness, address: &str, signature: &str, slot: u64, at: i64) {
    h.rpc.sigs.lock().entry(address.to_string()).or_default().insert(0, SignatureInfo { signature: signature.to_string(), slot, failed: false, block_time: Some(at) });
}

/// A buy of the pump.fun fixture's token by another wallet.
fn other_buy(h: &Harness, name: &str, buyer: &str, signature: &str, slot: u64, at: i64, mint: &str) {
    let mut tx = as_wallet(name, buyer, signature, at);
    tx["slot"] = json!(slot);
    h.rpc.put(&tx);
    list(h, mint, signature, slot, at);
}

async fn tracked() -> Harness {
    let h = harness();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.advance(30);
    h.work().await;
    h.tg.clear();
    h
}

const PUMP_SLOT: u64 = 300_100_000;
const T22_SLOT: u64 = 300_100_120;

#[tokio::test]
async fn wallets_that_buy_beside_an_entity_are_noted_and_twice_is_worth_a_look() {
    let h = tracked().await;
    let (shadow, slow, seller) = (addr(80), addr(81), addr(82));
    // Around the entity's pump.fun buy: one wallet in the same slot, one 40 seconds later.
    other_buy(&h, "pump_buy", &shadow, "sigCoBefore", PUMP_SLOT - 1, T0 - 1, MEME_MINT);
    list(&h, MEME_MINT, SIG_PUMP_BUY, PUMP_SLOT, T0);
    other_buy(&h, "pump_buy", &slow, "sigCoAfter", PUMP_SLOT + 100, T0 + 40, MEME_MINT);
    // A sell in the window is not a co-buy, and a buy five minutes later is outside it.
    let mut sell = as_wallet("jup_sell", &seller, "sigCoSell", T0 + 5);
    sell["slot"] = json!(PUMP_SLOT + 10);
    h.rpc.put(&sell);
    list(&h, MEME_MINT, "sigCoSell", PUMP_SLOT + 10, T0 + 5);
    other_buy(&h, "pump_buy", &addr(83), "sigCoLate", PUMP_SLOT + 800, T0 + 300, MEME_MINT);

    h.see(WALLET, &fixture("pump_buy")).await;
    h.advance(10);
    h.work().await;

    let near = h.store.evidence_of_wallet(&shadow).await.unwrap();
    assert_eq!((near[0].kind.as_str(), near[0].points, near[0].details["slots"].as_u64()), (kind::COBUY_SLOTS, 12, Some(1)));
    let far = h.store.evidence_of_wallet(&slow).await.unwrap();
    assert_eq!((far[0].kind.as_str(), far[0].points), (kind::COBUY_MINUTE, 6));
    assert_eq!(h.store.wallet(&shadow).await.unwrap().unwrap().state, WalletState::Seen, "once is not much");
    assert_eq!(h.store.wallet(&seller).await.unwrap(), None);
    assert_eq!(h.store.wallet(&addr(83)).await.unwrap(), None);
    assert!(h.rpc.calls.lock().iter().filter(|c| c.contains("sigCo")).all(|c| c.ends_with("forensics")), "co-buy reads are forensics");
    assert_eq!(h.store.job_counts().await.unwrap().0, 0);

    // The same wallet beside the same entity on a second token: now its funder is looked up.
    other_buy(&h, "t22_buy", &shadow, "sigCoT22", T22_SLOT + 2, T0 + 61, T22_MINT);
    list(&h, T22_MINT, SIG_T22_BUY, T22_SLOT, T0 + 60);
    h.see(WALLET, &fixture("t22_buy")).await;
    h.advance(10);
    assert!(h.work().await >= 2, "the scan, then the trace-back it queued");
    let wallet = h.store.wallet(&shadow).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.score), (WalletState::Seen, 24), "two tight co-buys");
    assert!(h.store.doc("funder", &shadow).await.unwrap().is_some());
    assert!(h.tg.sent().iter().all(|(_, text)| !text.contains("Possible new wallet")), "co-buys alone raise no alarm");
}

#[tokio::test]
async fn a_buy_made_in_a_crowd_says_nothing_about_anyone() {
    let h = tracked().await;
    list(&h, MEME_MINT, SIG_PUMP_BUY, PUMP_SLOT, T0);
    for i in 0..30u8 {
        other_buy(&h, "pump_buy", &addr(150 + i), &format!("sigCrowd{i}"), PUMP_SLOT + 1, T0 + 1, MEME_MINT);
    }
    h.see(WALLET, &fixture("pump_buy")).await;
    h.advance(10);
    h.work().await;
    assert!(h.store.doc("crowd", &format!("{MEME_MINT}:1")).await.unwrap().is_some());
    assert!(h.store.wallets_in_state(WalletState::Seen).await.unwrap().is_empty());
    assert_eq!(h.rpc.fetches().iter().filter(|c| c.contains("sigCrowd")).count(), 0, "a crowd is recognised from the listing, without reading it");
}

#[tokio::test]
async fn a_wallet_that_buys_beside_everyone_is_a_sniper_and_its_co_buys_are_withdrawn() {
    let h = tracked().await;
    let sniper = addr(84);
    h.say(ADMIN, &format!("/track {WALLET_2} shark")).await;
    h.say(ADMIN, &format!("/track {} orca", addr(85))).await;
    h.app.note_cobuy_for_test(&sniper, 1, MEME_MINT, 1).await;
    h.app.note_cobuy_for_test(&sniper, 2, MEME_MINT_2, 0).await;
    assert_eq!(h.store.wallet(&sniper).await.unwrap().unwrap().score, 12);
    h.app.note_cobuy_for_test(&sniper, 3, T22_MINT, 2).await;
    assert!(h.store.doc("sniper", &sniper).await.unwrap().is_some());
    assert!(h.store.evidence_of_wallet(&sniper).await.unwrap().iter().all(|e| e.voided), "its co-buys no longer count for anyone");
    assert_eq!(h.store.wallet(&sniper).await.unwrap().unwrap().score, 0);
    // And it stays ignored.
    h.app.note_cobuy_for_test(&sniper, 1, T22_MINT, 0).await;
    assert!(h.store.evidence_of_wallet(&sniper).await.unwrap().iter().all(|e| e.voided));
}

#[tokio::test]
async fn a_user_is_told_once_when_the_entities_they_follow_agree_on_a_token() {
    let h = tracked().await;
    h.say(ADMIN, &format!("/track {WALLET_2} shark")).await;
    h.store.upsert_account(FRIEND, "friend", false, T0).await.unwrap();
    h.store.set_follow(FRIEND, 1, FollowMode::Cards).await.unwrap();
    h.tg.clear();

    // One entity buys: no consensus for anyone.
    h.see(WALLET, &fixture("pump_buy")).await;
    assert!(h.inbox(ADMIN).contains("<b>BUY</b>") && !h.inbox(ADMIN).contains("Consensus"));
    // The second buys four minutes later. The admin follows both; the friend follows one.
    h.advance(240);
    h.see(WALLET_2, &as_wallet("pump_buy", WALLET_2, "sigSharkBuy", T0 + 240)).await;
    let notice = h.tg.sent_to(ADMIN).into_iter().find(|t| t.contains("Consensus")).expect("the admin is told");
    assert!(notice.contains("2 entities, 2 independent, within 10 min") && notice.contains("E#1 whale") && notice.contains("E#2 shark"), "{notice}");
    assert!(!h.inbox(FRIEND).contains("Consensus"), "one followed entity is not a consensus");
    // More buys in the same window do not repeat the alert.
    h.advance(40);
    h.see(WALLET_2, &as_wallet("pump_buy", WALLET_2, "sigSharkBuy2", T0 + 280)).await;
    assert_eq!(h.tg.sent_to(ADMIN).iter().filter(|t| t.contains("Consensus")).count(), 1);

    // A user who asked for three entities is not told at two.
    h.say(FRIEND, "/set entities 3").await;
    h.store.set_follow(FRIEND, 2, FollowMode::Cards).await.unwrap();
    let settings = h.app.trade_settings(FRIEND).await;
    assert_eq!(h.app.consensus_for(FRIEND, MEME_MINT, &settings).await.unwrap().independent, 2);
    assert_eq!(settings.min_entities, 3);
}

#[tokio::test]
async fn the_holder_map_groups_holders_by_funder_and_sets_pools_aside() {
    let h = tracked().await;
    let mint = MEME_MINT;
    let (a, b, c, pool, funder) = (addr(90), addr(91), addr(92), addr(93), addr(94));
    let token_accounts: Vec<String> = (0..4u8).map(|i| addr(95 + i)).collect();
    let owners = [&a, &b, &c, &pool];
    {
        let mut raw = h.rpc.raw.lock();
        raw.insert(format!("getTokenSupply:{mint}"), json!({ "value": { "amount": "1000" } }));
        raw.insert(
            format!("getTokenLargestAccounts:{mint}"),
            json!({ "value": [
            { "address": token_accounts[3], "amount": "400" }, { "address": token_accounts[0], "amount": "300" },
            { "address": token_accounts[1], "amount": "200" }, { "address": token_accounts[2], "amount": "100" } ] }),
        );
        let order = [3usize, 0, 1, 2];
        raw.insert(
            format!("getMultipleAccounts:{}", json!(order.iter().map(|i| token_accounts[*i].clone()).collect::<Vec<_>>())),
            json!({ "value": order.iter().map(|i| json!({ "data": { "parsed": { "info": { "owner": owners[*i] } } } })).collect::<Vec<_>>() }),
        );
        // Owners come back sorted; the pool's account belongs to a program.
        let mut sorted: Vec<&String> = owners.to_vec();
        sorted.sort();
        raw.insert(format!("getMultipleAccounts:{}", json!(sorted)), json!({ "value": sorted.iter().map(|o| json!({ "owner": if **o == pool { PUMP } else { SYSTEM } })).collect::<Vec<_>>() }));
    }
    // What the bot already knows: a and b share a funder; c's is unknown.
    for wallet in [&a, &b] {
        h.store.put_doc("funder", wallet, &json!({ "funder": funder }), T0).await.unwrap();
    }
    h.store.put_doc("funder", &c, &json!({ "funder": null }), T0).await.unwrap();

    let report = h.app.bubble_map(mint, Lane::Forensics).await.map_err(|_| "map").unwrap();
    assert_eq!(report.holders.len(), 4);
    assert_eq!(report.holders[0].service.as_deref(), Some("pool or program account"), "the largest holder is the pool");
    assert_eq!(report.clusters.len(), 1);
    assert_eq!((report.clusters[0].members.len(), report.clusters[0].amount), (2, 500));
    assert_eq!(report.largest_cluster_bps(), 5_000);
    // Kept for the trader's token screen.
    assert_eq!(h.store.doc("map", mint).await.unwrap().unwrap()["largest_bps"], json!(5_000));

    assert!(h.say(ADMIN, &format!("/map {mint}")).await[0].contains("Building the holder map"));
    h.work().await;
    let card = h.tg.last_to(ADMIN);
    for part in ["Holder map", "<b>Cluster 1</b> · 2 wallets · 50.0% of supply", "same funder", "40.0% · pool or program account", "A cluster is a question, not a verdict"] {
        assert!(card.contains(part), "missing {part:?} in:\n{card}");
    }
    h.say(ADMIN, &format!("/map {}", addr(99))).await;
    h.work().await;
    assert!(h.tg.last_to(ADMIN).contains("listed no holders"));
    assert!(h.say(ADMIN, "/map x").await[0].contains("Usage"));
}
