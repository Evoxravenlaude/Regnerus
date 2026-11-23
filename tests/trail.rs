//! The trail engine against a scripted chain: trace back, exit watch, the
//! deposit test and its tripwire, the exchange hop match, and the walk.

mod common;

use common::*;
use regnerus::identity::score::kind;
use regnerus::model::*;
use regnerus::obs::budget::Lane;
use regnerus::pipeline::Outcome;
use regnerus::store::Store;
use regnerus::trail::back::{hop_fit, is_round};
use serde_json::json;

const HOUR: i64 = 3_600;

async fn tracked() -> Harness {
    let h = harness();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.store.upsert_account(FRIEND, "friend", false, T0).await.unwrap();
    h.store.set_follow(FRIEND, 1, FollowMode::Cards).await.unwrap();
    // The root's own trace-back, queued by /track.
    h.advance(30);
    h.work().await;
    h.tg.clear();
    h
}

fn hot() -> String {
    addr(50)
}

fn kinds_of(rows: &[Evidence]) -> Vec<(&str, i32)> {
    rows.iter().filter(|e| !e.voided).map(|e| (e.kind.as_str(), e.points)).collect()
}

#[test]
fn a_withdrawal_fits_a_deposit_by_amount_and_time() {
    let deposit = 5_370_000_000u64;
    assert_eq!(hop_fit(deposit, 1_000, deposit - 5_000_000, 1_000 + 20 * 60, 6 * HOUR), Some(true), "a few thousandths short, twenty minutes later");
    assert_eq!(hop_fit(deposit, 1_000, deposit - 150_000_000, 1_000 + 20 * 60, 6 * HOUR), Some(false), "within three percent: a loose fit");
    assert_eq!(hop_fit(deposit, 1_000, deposit - 5_000_000, 1_000 + 3 * HOUR, 6 * HOUR), Some(false), "hours later: loose");
    assert_eq!(hop_fit(deposit, 1_000, deposit - 400_000_000, 1_000 + 60, 6 * HOUR), None, "too far short");
    assert_eq!(hop_fit(deposit, 1_000, deposit + 1, 1_000 + 60, 6 * HOUR), None, "more came out than went in");
    assert_eq!(hop_fit(deposit, 1_000, deposit, 1_000 + 7 * HOUR, 6 * HOUR), None, "after the window");
    assert_eq!(hop_fit(deposit, 1_000, deposit, 900, 6 * HOUR), None, "before the deposit");
    // Round amounts match by chance: never a tight fit.
    assert!(is_round(5 * SOL) && !is_round(deposit));
    assert_eq!(hop_fit(5 * SOL, 1_000, 5 * SOL - 5_000_000, 1_000 + 60, 6 * HOUR), Some(false));
}

#[tokio::test]
async fn a_wallet_first_funded_by_a_linked_wallet_becomes_a_candidate() {
    let h = tracked().await;
    let fresh = addr(10);
    h.rpc.transfer(WALLET, &fresh, 2 * SOL, "sigFundFresh", T0 + 10);
    h.rpc.transfer(&fresh, &addr(11), SOL / 2, "sigLater", T0 + 500);

    h.app.enqueue("trace_back", &fresh, None, json!({ "hops": 2 }), 0).await;
    h.work().await;

    let wallet = h.store.wallet(&fresh).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.score, wallet.entity_id), (WalletState::Candidate, 45, Some(1)));
    let rows = h.store.evidence_of_wallet(&fresh).await.unwrap();
    assert_eq!(kinds_of(&rows), vec![(kind::DIRECT_FUNDING, 45)]);
    assert_eq!(rows[0].signatures, vec!["sigFundFresh".to_string()], "the funding transaction is the evidence");
    assert!(h.tg.last_to(FRIEND).contains("Possible new wallet"));
    // The answer is kept: asked again, the chain is not read again.
    let funder = h.store.doc("funder", &fresh).await.unwrap().unwrap();
    assert_eq!((funder["funder"].as_str(), funder["lamports"].as_u64()), (Some(WALLET), Some(2 * SOL)));
    let reads = h.rpc.calls.lock().len();
    h.app.enqueue("trace_back", &fresh, None, json!({ "hops": 0 }), 0).await;
    h.work().await;
    assert_eq!(h.rpc.calls.lock().len(), reads, "the second look-up came from the cache");
    assert!(h.rpc.calls.lock().iter().filter(|c| c.starts_with("raw:") || c.starts_with("tx:")).all(|c| c.ends_with("forensics") || c.ends_with("watch")), "forensics never rides the copy lane");
}

#[tokio::test]
async fn the_wallet_that_funded_a_tracked_wallet_is_surfaced_as_its_possible_parent() {
    let h = harness();
    let (parent, grandparent) = (addr(15), addr(16));
    h.rpc.transfer(&grandparent, &parent, 50 * SOL, "sigGrand", T0 - 20_000);
    h.rpc.transfer(&parent, WALLET, 10 * SOL, "sigParent", T0 - 10_000);
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.tg.clear();
    assert_eq!(h.work().await, 0, "the trace-back waits a moment after tracking");
    h.advance(30);
    h.work().await;

    // The parent is proposed, with the funding transaction as its evidence. It is not linked.
    let wallet = h.store.wallet(&parent).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.score, wallet.entity_id), (WalletState::Candidate, 45, Some(1)));
    let rows = h.store.evidence_of_wallet(&parent).await.unwrap();
    assert_eq!((kinds_of(&rows), rows[0].signatures.clone()), (vec![(kind::FUNDED_LINKED, 45)], vec!["sigParent".to_string()]));
    let notice = h.tg.last_to(ADMIN);
    assert!(notice.contains("Possible new wallet") && notice.contains("it gave a linked wallet its first SOL <b>+45</b>"), "{notice}");
    // Its own funder is looked up next, but a grandparent is not proposed on that alone.
    assert!(h.store.doc("funder", &parent).await.unwrap().is_some());
    assert_eq!(h.store.wallet(&grandparent).await.unwrap(), None);
    // A labelled funder is a service, not a parent.
    let h = harness();
    h.label(&parent, LabelClass::Onramp, "MoonPay");
    h.rpc.transfer(&parent, WALLET, 10 * SOL, "sigParent", T0 - 10_000);
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.advance(30);
    h.work().await;
    assert_eq!(h.store.wallet(&parent).await.unwrap(), None);
}

#[tokio::test]
async fn a_plain_funder_is_followed_upward_until_the_hop_limit() {
    let h = tracked().await;
    let (top, middle, bottom) = (addr(12), addr(13), addr(14));
    h.rpc.transfer(WALLET, &top, 3 * SOL, "sigTop", T0 + 1);
    h.rpc.transfer(&top, &middle, 2 * SOL, "sigMiddle", T0 + 2);
    h.rpc.transfer(&middle, &bottom, SOL, "sigBottom", T0 + 3);

    // One hop is not enough to reach the linked wallet from the bottom.
    h.app.enqueue("trace_back", &bottom, None, json!({ "hops": 1 }), 0).await;
    h.work().await;
    assert_eq!(h.store.wallet(&top).await.unwrap(), None, "the walk stopped at the middle");
    assert!(h.store.doc("funder", &middle).await.unwrap().is_some());
    // Two hops reach it: the wallet the entity funded is the candidate, not the ones below.
    h.app.enqueue("trace_back", &bottom, None, json!({ "hops": 2 }), 0).await;
    h.work().await;
    assert_eq!(h.store.wallet(&top).await.unwrap().unwrap().state, WalletState::Candidate);
    assert_eq!(h.store.wallet(&middle).await.unwrap(), None);
    assert_eq!(h.store.wallet(&bottom).await.unwrap(), None);
}

/// E#1's root pays into a deposit address that another wallet also uses.
async fn with_deposit(h: &Harness, deposit: &str, lamports: u64) -> String {
    let sibling = addr(21);
    // Hot wallets are numbered; the exchange is the first word of the label.
    h.label(&hot(), LabelClass::CexHot, "Binance 2");
    h.rpc.transfer(&sibling, deposit, 3 * SOL, "sigSiblingIn", T0 - 5_000);
    h.rpc.transfer(deposit, &hot(), 3 * SOL, "sigSweep1", T0 - 4_900);
    h.rpc.transfer(WALLET, deposit, lamports, "sigDeposit", T0 + 40);
    h.rpc.transfer(deposit, &hot(), lamports, "sigSweep2", T0 + 45);
    assert_eq!(h.app.process_signature(WALLET, "sigDeposit", true, false).await, Outcome::Transfers(1));
    settle().await;
    sibling
}

#[tokio::test]
async fn paying_into_the_same_deposit_address_is_proof_and_the_address_becomes_a_tripwire() {
    let h = tracked().await;
    let deposit = addr(20);
    let sibling = with_deposit(&h, &deposit, 5 * SOL).await;

    // Nothing is decided at once: the sweep needs a moment to happen.
    assert_eq!(h.work().await, 0);
    assert_eq!(h.store.deposit(&deposit).await.unwrap(), None);
    h.advance(120);
    h.work().await;

    let tagged = h.store.deposit(&deposit).await.unwrap().unwrap();
    assert_eq!((tagged.entity_id, tagged.exchange.as_str(), tagged.status, tagged.senders), (1, "Binance", DepositStatus::Qualified, 2));
    // The wallet that already paid in is the entity's: linked outright, and announced.
    let wallet = h.store.wallet(&sibling).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.linked_by, wallet.entity_id), (WalletState::Linked, Some(LinkedBy::Proof), Some(1)));
    let notice = h.tg.last_to(FRIEND);
    assert!(notice.contains("New wallet linked") && notice.contains("by proof"), "{notice}");
    assert_eq!(h.app.proof_count().await, 1);

    // The stream now watches the deposit address and the new wallet.
    use regnerus::chain::solana::stream::AddressSource;
    let watched = h.app.addresses().await;
    assert!(watched.contains(&deposit) && watched.contains(&sibling) && watched.contains(&WALLET.to_string()), "{watched:?}");
    let card = h.say(ADMIN, "/entity 1").await;
    assert!(card[0].contains("<b>Deposit addresses (1)</b>") && card[0].contains("Binance · 2 senders · tripwire"), "{}", card[0]);

    // Weeks later a wallet nobody has seen pays in. That is the new-wallet alert.
    h.tg.clear();
    let stranger = addr(22);
    h.rpc.transfer(&stranger, &deposit, 7 * SOL, "sigTripwire", T0 + 9_000);
    assert_eq!(h.app.process_signature(&deposit, "sigTripwire", true, false).await, Outcome::Transfers(1));
    settle().await;
    let wallet = h.store.wallet(&stranger).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.linked_by), (WalletState::Linked, Some(LinkedBy::Proof)));
    assert!(h.tg.last_to(ADMIN).contains("New wallet linked"));
    assert_eq!(h.app.proof_count().await, 2);
    // The sweep out of the deposit address is not a payment in: no proof for the exchange.
    assert_eq!(h.app.process_signature(&deposit, "sigSweep2", true, false).await, Outcome::Transfers(0));
    assert_eq!(h.store.wallet(&hot()).await.unwrap(), None);

    // A payment that arrived while the stream was down is found by the backfill.
    h.app.backfill_all().await; // the first pass only marks where the address's history stands
    let late = addr(25);
    h.rpc.transfer(&late, &deposit, 2 * SOL, "sigWhileDown", T0 + 9_200);
    assert!(h.app.backfill_all().await >= 1);
    settle().await;
    assert_eq!(h.store.wallet(&late).await.unwrap().unwrap().linked_by, Some(LinkedBy::Proof));

    // The entity's own later deposits to it are recorded for hop matching, not re-inspected.
    h.rpc.transfer(WALLET, &deposit, 4 * SOL, "sigDepositAgain", T0 + 9_500);
    h.app.process_signature(WALLET, "sigDepositAgain", true, false).await;
    assert_eq!(h.store.docs("cexflow", "Binance:1:", 10).await.unwrap().len(), 2);
    assert_eq!(h.store.job_counts().await.unwrap().0, 0);
}

#[tokio::test]
async fn an_address_with_many_senders_is_not_a_personal_deposit_address() {
    let h = tracked().await;
    let processor = addr(23);
    h.label(&hot(), LabelClass::CexHot, "Binance");
    for i in 0..9u8 {
        h.rpc.transfer(&addr(100 + i), &processor, SOL, &format!("sigIn{i}"), T0 - 1_000 + i as i64);
    }
    h.rpc.transfer(WALLET, &processor, 5 * SOL, "sigPay", T0 + 40);
    h.rpc.transfer(&processor, &hot(), 14 * SOL, "sigSweep", T0 + 50);
    h.app.process_signature(WALLET, "sigPay", true, false).await;
    h.advance(120);
    h.work().await;

    let tagged = h.store.deposit(&processor).await.unwrap().unwrap();
    assert_eq!((tagged.status, tagged.senders), (DepositStatus::Rejected, 10));
    // None of the strangers who paid the same processor is touched.
    for i in 0..9u8 {
        assert_eq!(h.store.wallet(&addr(100 + i)).await.unwrap(), None);
    }
    assert!(h.tg.sent().is_empty());
    use regnerus::chain::solana::stream::AddressSource;
    assert!(!h.app.addresses().await.contains(&processor), "not a tripwire");
    assert!(h.say(ADMIN, "/entity 1").await[0].contains("too many senders"));
}

#[tokio::test]
async fn an_address_that_pays_a_crowd_is_learned_as_a_hub_and_left_alone() {
    let h = tracked().await;
    let hub = addr(24);
    for i in 0..35u8 {
        h.rpc.transfer(&hub, &addr(120 + i), SOL / 10, &format!("sigOut{i}"), T0 - 500 + i as i64);
    }
    h.rpc.transfer(WALLET, &hub, 5 * SOL, "sigToHub", T0 + 40);
    h.app.process_signature(WALLET, "sigToHub", true, false).await;
    h.advance(120);
    h.work().await;
    assert_eq!(h.app.labels.get(&hub).map(|l| l.class), Some(LabelClass::Hub));
    assert_eq!(h.store.labels().await.unwrap().iter().filter(|l| l.address == hub).count(), 1, "remembered across restarts");
    assert!(h.store.wallets_in_state(WalletState::Seen).await.unwrap().is_empty());
    // The next payment to it is not even queued.
    h.rpc.transfer(WALLET, &hub, 6 * SOL, "sigToHubAgain", T0 + 400);
    h.app.process_signature(WALLET, "sigToHubAgain", true, false).await;
    assert_eq!(h.store.job_counts().await.unwrap().0, 0);
}

#[tokio::test]
async fn sol_sent_to_a_fresh_wallet_marks_a_successor_once_it_is_clear_it_is_not_a_deposit() {
    let h = tracked().await;
    let successor = addr(30);
    h.rpc.transfer(WALLET, &successor, 2 * SOL, "sigSeed", T0 + 40);
    h.app.process_signature(WALLET, "sigSeed", true, false).await;
    // Small change is under the volume filter and is not followed at all.
    h.rpc.transfer(WALLET, &addr(31), SOL / 10, "sigDust", T0 + 41);
    h.app.process_signature(WALLET, "sigDust", true, false).await;
    assert_eq!(h.store.job_counts().await.unwrap().0, 1);

    // Idle so far: it could still be a deposit waiting to be swept. Three looks say nothing.
    for _ in 0..3 {
        h.advance(1_000);
        h.work().await;
        assert_eq!(h.store.wallet(&successor).await.unwrap(), None);
    }
    // Still idle on the last look: a wallet, funded first by the entity.
    h.advance(1_000);
    h.work().await;
    let wallet = h.store.wallet(&successor).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.score), (WalletState::Candidate, 45));
    assert_eq!(kinds_of(&h.store.evidence_of_wallet(&successor).await.unwrap()), vec![(kind::DIRECT_FUNDING, 45)]);
}

#[tokio::test]
async fn tokens_handed_to_a_wallet_that_sells_them_is_the_strongest_funding_signal() {
    let h = tracked().await;
    let receiver = addr(32);
    // The entity buys the token...
    h.see(WALLET, &fixture("pump_buy")).await;
    // ...sends some to another wallet...
    let handoff: serde_json::Value = serde_json::from_str(&serde_json::to_string(&fixture("token_out")).unwrap().replace(OTHER, &receiver)).unwrap();
    assert_eq!(h.see(WALLET, &handoff).await, Outcome::Transfers(1));
    // ...which has not sold yet: nothing to say, look again later.
    h.rpc.push_sig(&receiver, SIG_TOKEN_OUT, false);
    h.advance(120);
    h.work().await;
    assert_eq!(h.store.wallet(&receiver).await.unwrap(), None);
    assert_eq!(h.store.job_counts().await.unwrap().0, 1, "queued to look again");

    // It sells.
    let sell = as_wallet("jup_sell", &receiver, "sigReceiverSells", T0 + 600);
    h.rpc.put(&sell);
    h.rpc.push_sig(&receiver, "sigReceiverSells", false);
    h.advance(2_000);
    h.work().await;
    let wallet = h.store.wallet(&receiver).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.score), (WalletState::Candidate, 60));
    assert_eq!(kinds_of(&h.store.evidence_of_wallet(&receiver).await.unwrap()), vec![(kind::TOKEN_HANDOFF, 60)]);
}

#[tokio::test]
async fn an_exchange_withdrawal_that_matches_a_deposit_is_a_hop() {
    let h = tracked().await;
    let deposit = addr(20);
    let deposited = 5_370_000_000u64;
    with_deposit(&h, &deposit, deposited).await;
    h.advance(120);
    h.work().await;
    assert_eq!(h.store.docs("cexflow", "Binance:1:", 10).await.unwrap().len(), 1);
    h.tg.clear();

    // Twenty minutes after the deposit, the exchange funds a wallet with no history.
    let fresh = addr(40);
    h.rpc.transfer(&hot(), &fresh, deposited - 5_000_000, "sigWithdrawal", T0 + 40 + 20 * 60);
    h.app.enqueue("trace_back", &fresh, None, json!({ "hops": 2 }), 0).await;
    h.work().await;
    let rows = h.store.evidence_of_wallet(&fresh).await.unwrap();
    assert_eq!(kinds_of(&rows), vec![(kind::CEX_HOP_TIGHT, 40)]);
    assert_eq!(rows[0].details["fits"], json!(1));
    assert_eq!(h.store.wallet(&fresh).await.unwrap().unwrap().state, WalletState::Candidate);
    assert!(h.tg.last_to(FRIEND).contains("exchange withdrawal matching a deposit closely <b>+40</b>"), "{}", h.tg.last_to(FRIEND));

    // A second wallet fits the same deposit: neither match means as much any more.
    let also = addr(41);
    h.rpc.transfer(&hot(), &also, deposited - 60_000_000, "sigWithdrawal2", T0 + 40 + 50 * 60);
    h.app.enqueue("trace_back", &also, None, json!({ "hops": 2 }), 0).await;
    h.work().await;
    for wallet in [&fresh, &also] {
        assert_eq!(kinds_of(&h.store.evidence_of_wallet(wallet).await.unwrap()), vec![(kind::CEX_HOP_LOOSE, 20)], "{wallet}");
        assert_eq!(h.store.wallet(wallet).await.unwrap().unwrap().state, WalletState::Seen);
    }
    // A withdrawal a day later is nobody's hop.
    let late = addr(42);
    h.rpc.transfer(&hot(), &late, deposited - 5_000_000, "sigLate", T0 + 86_400);
    h.app.enqueue("trace_back", &late, None, json!({ "hops": 2 }), 0).await;
    h.work().await;
    assert_eq!(h.store.wallet(&late).await.unwrap(), None);
    assert!(h.store.doc("cexfund", &format!("Binance:{late}")).await.unwrap().is_some(), "but it is remembered as exchange-funded");
}

#[tokio::test]
async fn the_walk_follows_the_largest_flows_to_an_exchange_and_tags_what_it_finds() {
    let h = tracked().await;
    h.label(&hot(), LabelClass::CexHot, "Binance");
    let (relay, far_deposit, other_sender, funder, near_deposit, co_sender) = (addr(60), addr(61), addr(62), addr(63), addr(64), addr(65));
    // In: the exchange funded a wallet that funded the target.
    h.rpc.transfer(&hot(), &funder, 20 * SOL, "sigW1", T0 - 9_000);
    h.rpc.transfer(&funder, WALLET, 20 * SOL, "sigW2", T0 - 8_000);
    // Out, two hops: target -> relay -> a deposit address someone else also uses.
    h.rpc.transfer(WALLET, &relay, 10 * SOL, "sigW3", T0 - 7_000);
    h.rpc.transfer(&relay, &far_deposit, 10 * SOL, "sigW4", T0 - 6_000);
    h.rpc.transfer(&other_sender, &far_deposit, 4 * SOL, "sigW5", T0 - 5_500);
    h.rpc.transfer(&far_deposit, &hot(), 14 * SOL, "sigW6", T0 - 5_000);
    // Out, one hop: the target's own deposit address, shared with one more wallet.
    h.rpc.transfer(&co_sender, &near_deposit, 6 * SOL, "sigW7", T0 - 4_000);
    h.rpc.transfer(WALLET, &near_deposit, 8 * SOL, "sigW8", T0 - 3_000);
    h.rpc.transfer(&near_deposit, &hot(), 14 * SOL, "sigW9", T0 - 2_000);
    // Noise under the volume filter.
    h.rpc.transfer(WALLET, &addr(66), SOL, "sigW10", T0 - 1_000);

    let reply = h.say(FRIEND, &format!("/trail {WALLET} 30 5")).await;
    assert!(reply[0].contains("Walking the trail") && reply[0].contains("30 days"), "{}", reply[0]);
    h.work().await;

    let card = h.tg.last_to(FRIEND);
    for part in [
        "Trail of",
        "Exchange deposit addresses reached",
        "Binance · 1 hop away · 2 senders · personal",
        "Binance · 2 hops away · 2 senders · personal",
        "Funded from an exchange",
        "← Binance · 20 SOL",
        "Largest flows followed",
        "Tagged 2 deposit addresses for E#1",
    ] {
        assert!(card.contains(part), "missing {part:?} in:\n{card}");
    }
    assert!(!card.contains(&regnerus::util::short(&addr(66))), "flows under the filter are not followed:\n{card}");

    // The address the target paid into itself: a tripwire, and its co-sender is linked by proof.
    assert_eq!(h.store.deposit(&near_deposit).await.unwrap().unwrap().status, DepositStatus::Qualified);
    assert_eq!(h.store.wallet(&co_sender).await.unwrap().unwrap().linked_by, Some(LinkedBy::Proof));
    // The address two hops out is a lead: its senders get evidence, not a link.
    assert_eq!(h.store.deposit(&far_deposit).await.unwrap().unwrap().status, DepositStatus::Pending);
    for lead in [&relay, &other_sender] {
        let wallet = h.store.wallet(lead).await.unwrap().unwrap();
        assert_eq!((wallet.state, wallet.score), (WalletState::Seen, 35), "{lead}");
    }
    use regnerus::chain::solana::stream::AddressSource;
    let watched = h.app.addresses().await;
    assert!(watched.contains(&near_deposit) && !watched.contains(&far_deposit));

    // The same walk on a wallet no entity owns reports, and tags nothing.
    h.tg.clear();
    h.say(FRIEND, &format!("/trail {relay}")).await;
    h.work().await;
    assert!(h.tg.last_to(FRIEND).contains("not linked to an entity, so nothing was tagged"), "{}", h.tg.last_to(FRIEND));
    assert!(h.say(FRIEND, &format!("/trail {}", hot())).await[0].contains("labelled as a service"));
    assert!(h.say(FRIEND, "/trail nope").await[0].contains("Usage"));
}

#[tokio::test]
async fn a_walk_stops_at_its_credit_cap_and_says_so() {
    let h = harness_with(&[("WALK_CREDITS", "5"), ("WALK_HOPS", "8")]);
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    // A chain of plain wallets, each passing 10 SOL on.
    let mut from = WALLET.to_string();
    for i in 0..10u8 {
        let to = addr(70 + i);
        h.rpc.transfer(&from, &to, 10 * SOL, &format!("sigChain{i}"), T0 - 900 + i as i64);
        from = to;
    }
    let report = h.app.walk(WALLET, 30, 5 * SOL).await.unwrap();
    assert!(report.cut_short.as_deref().is_some_and(|why| why.contains("credit cap of 5")), "{report:?}");
    assert!(report.visited < 10 && report.credits >= 5, "{report:?}");
    assert!(report.deposits.is_empty());
    // Sampling one wallet costs one listing plus one call per transaction read.
    let sample = h.app.sample_wallet(&addr(70), 40, 25, None, Lane::Forensics).await.unwrap();
    assert_eq!((sample.signatures, sample.credits, sample.flows.len(), sample.complete), (2, 3, 2, true));
}
