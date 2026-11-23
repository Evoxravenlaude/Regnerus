//! Attaching wallets to entities: the arithmetic, the states, the notices,
//! and the admin's corrections.

mod common;

use common::*;
use regnerus::identity::Unique;
use regnerus::identity::behaviour::{fingerprint_points, rotated};
use regnerus::identity::score::{Verdict, family_of, judge, kind};
use regnerus::model::*;
use regnerus::store::Store;
use serde_json::json;

fn row(kind_name: &str, points: i32) -> Evidence {
    Evidence {
        id: 1,
        wallet: OTHER.into(),
        entity_id: 1,
        kind: kind_name.into(),
        family: family_of(kind_name).into(),
        points,
        signatures: vec![],
        details: json!({}),
        source: "script".into(),
        voided: false,
        observed_at: 0,
    }
}

fn verdict(rows: &[Evidence]) -> Verdict {
    judge(rows, false)
}

#[test]
fn a_score_is_the_sum_of_its_families_each_under_its_cap() {
    assert_eq!(verdict(&[]).state, WalletState::Seen);
    let one = verdict(&[row(kind::COBUY_SLOTS, 12)]);
    assert_eq!((one.state, one.score), (WalletState::Seen, 12));

    // Five tight co-buys are worth 36, not 60.
    let cobuys: Vec<Evidence> = (0..5).map(|_| row(kind::COBUY_SLOTS, 12)).collect();
    let capped = verdict(&cobuys);
    assert_eq!((capped.score, capped.families["cobuy"], capped.state), (36, 36, WalletState::Seen));

    let funded = verdict(&[row(kind::DIRECT_FUNDING, 45)]);
    assert_eq!((funded.state, funded.score, funded.funding), (WalletState::Candidate, 45, true));

    // Funding plus enough behaviour links, and says it was the score.
    let mut rows = cobuys.clone();
    rows.push(row(kind::DIRECT_FUNDING, 45));
    let linked = verdict(&rows);
    assert_eq!((linked.state, linked.linked_by, linked.score), (WalletState::Linked, Some(LinkedBy::Score), 81));

    // Weak evidence against takes it back under the line.
    let against = verdict(&[row(kind::DIRECT_FUNDING, 45), row(kind::OTHER_DEPOSIT, -10)]);
    assert_eq!((against.state, against.score), (WalletState::Seen, 35));

    // A voided row counts for nothing.
    let mut voided = row(kind::DIRECT_FUNDING, 45);
    voided.voided = true;
    assert_eq!(verdict(&[voided]).state, WalletState::Seen);
}

#[test]
fn behaviour_alone_never_links_a_wallet() {
    let mut rows: Vec<Evidence> = (0..3).map(|_| row(kind::COBUY_SLOTS, 12)).collect();
    rows.extend([row(kind::FINGERPRINT, 15), row(kind::HABIT, 10), row(kind::ROTATION, 8)]);
    let all = verdict(&rows);
    assert_eq!((all.state, all.score, all.funding), (WalletState::Candidate, 69, false), "every behaviour family at its cap is one short of a link");

    // Not even with the reviewer's vote on top.
    rows.push(row(kind::AI_REVIEW, 15));
    let reviewed = verdict(&rows);
    assert_eq!((reviewed.state, reviewed.score), (WalletState::Candidate, 69));

    // Over-full families do not sneak past the cap either.
    rows.extend([row(kind::FINGERPRINT, 15), row(kind::HABIT, 10), row(kind::ROTATION, 8)]);
    assert_eq!(verdict(&rows).state, WalletState::Candidate);
}

#[test]
fn proof_admins_and_the_reviewer_each_have_their_own_weight() {
    let proof = verdict(&[row(kind::DEPOSIT_PROOF, 100)]);
    assert_eq!((proof.state, proof.linked_by, proof.score), (WalletState::Linked, Some(LinkedBy::Proof), 100));
    assert!(proof.trusted(false), "proof is trusted from the start");

    // The reviewer can carry a funded wallet over the line, and that is marked.
    let grey = [row(kind::DIRECT_FUNDING, 45), row(kind::FINGERPRINT, 15)];
    assert_eq!(verdict(&grey).score, 60);
    let mut with_link = grey.to_vec();
    with_link.push(row(kind::AI_REVIEW, 15));
    let pushed = verdict(&with_link);
    assert_eq!((pushed.state, pushed.linked_by, pushed.score), (WalletState::Linked, Some(LinkedBy::Review), 75));
    assert!(!pushed.trusted(true), "a link the reviewer carried is never auto-copied");
    let mut with_reject = grey.to_vec();
    with_reject.push(row(kind::AI_REVIEW, -25));
    assert_eq!((verdict(&with_reject).state, verdict(&with_reject).score), (WalletState::Seen, 35));

    // A score link is trusted only once calibration has enough proofs.
    let scored = verdict(&[row(kind::TOKEN_HANDOFF, 60), row(kind::COBUY_SLOTS, 12)]);
    assert_eq!(scored.linked_by, Some(LinkedBy::Score));
    assert!(!scored.trusted(false) && scored.trusted(true));

    // "Not them" holds against everything but an admin.
    let rejected = judge(&[row(kind::DEPOSIT_PROOF, 100), row(kind::DIRECT_FUNDING, 45)], true);
    assert_eq!((rejected.state, rejected.score), (WalletState::Rejected, 0));
    let overruled = judge(&[row(kind::ADMIN_CONFIRM, 100)], true);
    assert_eq!((overruled.state, overruled.linked_by), (WalletState::Linked, Some(LinkedBy::Admin)));
    assert_eq!(verdict(&[row(kind::ROOT, 100)]).linked_by, Some(LinkedBy::Root));
}

fn print(app: Option<&str>, cu_price: Option<u64>, cu_limit: Option<u32>, tip: u64) -> Fingerprint {
    Fingerprint {
        fee_payer: "x".into(),
        self_paid: true,
        wallet_signed: true,
        cu_limit,
        cu_price,
        priority_fee_lamports: 0,
        tip_lamports: tip,
        tip_account: None,
        tip_family: (tip > 0).then(|| "jito".to_string()),
        app: app.map(str::to_string),
        app_fee_lamports: 0,
    }
}

#[test]
fn a_fingerprint_matches_on_exact_presets_and_not_on_an_apps_own_signature() {
    let entity = vec![print(Some("Axiom"), Some(1_000_000), Some(80_000), 1_000_000); 3];
    assert_eq!(fingerprint_points(&entity[..2], &entity), 15, "app, price, limit and tip all match");
    let other_app = vec![print(Some("Photon"), Some(1_000_000), Some(80_000), 0); 2];
    assert_eq!(fingerprint_points(&other_app, &entity), 6, "price and limit only");
    let nothing = vec![print(None, Some(5), Some(7), 0); 2];
    assert_eq!(fingerprint_points(&nothing, &entity), 0);
    assert_eq!(fingerprint_points(&entity[..1], &entity), 0, "one trade is not a habit");

    // Trades an app signs for its users all look alike: no points.
    let mut signed_by_app = entity.clone();
    for p in &mut signed_by_app {
        p.self_paid = false;
    }
    assert_eq!(fingerprint_points(&signed_by_app, &entity), 0);
}

#[test]
fn rotation_needs_the_old_wallet_to_have_gone_quiet() {
    let hour = 3_600;
    assert!(rotated(100 * hour, &[98 * hour], 110 * hour), "the old wallet stopped two hours before and has been quiet for twelve");
    assert!(!rotated(100 * hour, &[98 * hour], 101 * hour), "too soon to call it quiet");
    assert!(!rotated(100 * hour, &[80 * hour], 200 * hour), "it stopped long before this one started");
    assert!(!rotated(100 * hour, &[], 200 * hour));
}

async fn tracked() -> Harness {
    let h = harness();
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.store.upsert_account(FRIEND, "friend", false, T0).await.unwrap();
    h.store.set_follow(FRIEND, 1, FollowMode::Cards).await.unwrap();
    h.tg.clear();
    h
}

#[tokio::test]
async fn a_wallet_climbs_from_seen_to_candidate_to_linked_and_followers_are_told_each_time() {
    let h = tracked().await;
    let app = &h.app;

    // One co-buy: seen, and nobody is told.
    let change = app.add_evidence(WALLET_2, 1, kind::COBUY_SLOTS, None, vec!["sigA".into()], json!({ "mint": MEME_MINT }), Unique::Key(MEME_MINT.into())).await.unwrap().unwrap();
    assert_eq!((change.after.state, change.after.score), (WalletState::Seen, 12));
    assert!(h.tg.sent().is_empty());
    assert!(!h.store.watched_wallets().await.unwrap().iter().any(|w| w.address == WALLET_2), "a seen wallet is not subscribed");
    // The same co-buy again adds nothing.
    assert_eq!(app.add_evidence(WALLET_2, 1, kind::COBUY_SLOTS, None, vec![], json!({}), Unique::Key(MEME_MINT.into())).await.unwrap(), None);

    // Direct funding: 57, a candidate. Both followers get the notice with its arithmetic.
    app.add_evidence(WALLET_2, 1, kind::DIRECT_FUNDING, None, vec!["sigFund".into()], json!({}), Unique::Key("sigFund".into())).await.unwrap().unwrap();
    settle().await;
    let wallet = h.store.wallet(WALLET_2).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.score, wallet.entity_id, wallet.linked_by), (WalletState::Candidate, 57, Some(1), None));
    for chat in [ADMIN, FRIEND] {
        let notice = h.tg.last_to(chat);
        for part in
            ["Possible new wallet", "E#1 whale", "score <b>57</b>", "first SOL came from a linked wallet <b>+45</b>", "within 3 slots <b>+12</b>", "cobuy +12 (cap 36)", "funding +45", WALLET_2]
        {
            assert!(notice.contains(part), "missing {part:?} in:\n{notice}");
        }
    }
    let buttons: Vec<String> = h.tg.last_buttons(FRIEND).into_iter().map(|(text, _)| text).collect();
    assert_eq!(buttons, vec!["Confirm", "Not them", "Review", "Watch only"]);
    assert!(h.store.watched_wallets().await.unwrap().iter().any(|w| w.address == WALLET_2), "a candidate is watched live");
    // The score it was first proposed at is kept for calibration.
    let snapshot = h.store.doc("snapshot", WALLET_2).await.unwrap().unwrap();
    assert_eq!((snapshot["score"].as_i64(), snapshot["entity"].as_i64()), (Some(57), Some(1)));
    assert_eq!(h.store.job_counts().await.unwrap().0, 2, "its funder is looked up, as the root's was");

    // A candidate's trades send no cards.
    h.tg.clear();
    h.see(WALLET_2, &as_wallet("pump_buy", WALLET_2, "sigCandidateBuy", T0)).await;
    assert!(h.tg.sent().iter().all(|(_, text)| !text.contains("<b>BUY</b>")), "{:?}", h.tg.sent());

    // Two more co-buys take it to 81 with a funding signal: linked.
    h.tg.clear();
    app.add_evidence(WALLET_2, 1, kind::COBUY_SLOTS, None, vec![], json!({}), Unique::Key(MEME_MINT_2.into())).await.unwrap();
    let change = app.add_evidence(WALLET_2, 1, kind::COBUY_SLOTS, None, vec![], json!({}), Unique::Key(T22_MINT.into())).await.unwrap().unwrap();
    assert_eq!((change.after.state, change.after.linked_by, change.after.score), (WalletState::Linked, Some(LinkedBy::Score), 81));
    let notice = h.tg.last_to(FRIEND);
    assert!(notice.contains("New wallet linked") && notice.contains("by score (81)"), "{notice}");
    assert_eq!(
        h.tg.last_buttons(FRIEND).iter().map(|(t, d)| (t.as_str(), d.as_str())).collect::<Vec<_>>(),
        vec![("Why", format!("y:{WALLET_2}").as_str()), ("Unlink", format!("n:{WALLET_2}").as_str()), ("E#1", "e:1")]
    );

    // Now its trades are the entity's, and they send cards.
    h.tg.clear();
    h.see(WALLET_2, &as_wallet("jup_sell", WALLET_2, "sigLinkedSell", T0 + 1)).await;
    assert!(h.tg.last_to(FRIEND).contains("<b>SELL</b> · <b>E#1 whale</b>"), "{}", h.tg.last_to(FRIEND));
}

#[tokio::test]
async fn the_buttons_under_a_notice_settle_it_and_only_an_admin_may_press_them() {
    let h = tracked().await;
    h.app.add_evidence(OTHER, 1, kind::DIRECT_FUNDING, None, vec![], json!({}), Unique::Key("f".into())).await.unwrap();
    h.tg.clear();

    // A follower who is not an admin cannot decide for everyone.
    h.press(FRIEND, &format!("n:{OTHER}"), 7).await;
    h.press(FRIEND, &format!("c:{OTHER}"), 7).await;
    assert_eq!(h.tg.toasts(), vec!["Only an admin can do that.", "Only an admin can confirm a wallet."]);
    assert_eq!(h.store.wallet(OTHER).await.unwrap().unwrap().state, WalletState::Candidate);
    h.press(FRIEND, &format!("w:{OTHER}"), 7).await;
    assert!(h.tg.toasts()[2].contains("stays a candidate"));

    // Not them: rejected, unwatched, and not proposed again whatever turns up.
    h.press(ADMIN, &format!("n:{OTHER}"), 7).await;
    assert!(h.tg.toasts()[3].contains("will not be proposed for it again"), "{:?}", h.tg.toasts());
    assert_eq!(h.store.wallet(OTHER).await.unwrap().unwrap().state, WalletState::Rejected);
    assert!(!h.store.watched_wallets().await.unwrap().iter().any(|w| w.address == OTHER));
    h.tg.clear();
    let change = h.app.add_evidence(OTHER, 1, kind::TOKEN_HANDOFF, None, vec![], json!({}), Unique::Key("h".into())).await.unwrap().unwrap();
    assert_eq!(change.after.state, WalletState::Rejected);
    assert!(h.tg.sent().is_empty(), "no notice for a wallet an admin has ruled out");

    // Confirm undoes it.
    h.press(ADMIN, &format!("c:{OTHER}"), 7).await;
    let wallet = h.store.wallet(OTHER).await.unwrap().unwrap();
    assert_eq!((wallet.state, wallet.linked_by), (WalletState::Linked, Some(LinkedBy::Admin)));
    assert!(h.tg.last_to(FRIEND).contains("by an admin"));

    // "Why" opens the case file as a new message.
    h.tg.clear();
    h.press(FRIEND, &format!("y:{OTHER}"), 7).await;
    assert!(h.tg.last_to(FRIEND).contains("Evidence"), "{}", h.tg.last_to(FRIEND));
}

#[tokio::test]
async fn a_labelled_service_is_never_given_evidence() {
    let h = tracked().await;
    h.label(OTHER, LabelClass::CexHot, "Binance");
    assert_eq!(h.app.add_evidence(OTHER, 1, kind::DIRECT_FUNDING, None, vec![], json!({}), Unique::Key("f".into())).await.unwrap(), None);
    assert_eq!(h.store.wallet(OTHER).await.unwrap(), None);
}

#[tokio::test]
async fn a_wallet_belongs_to_one_entity_and_a_second_claim_asks_for_a_merge() {
    let h = tracked().await;
    h.say(ADMIN, &format!("/track {WALLET_2} shark")).await;
    h.tg.clear();
    // OTHER is linked to E#1 by proof.
    h.app.add_evidence(OTHER, 1, kind::DEPOSIT_PROOF, Some(100), vec![], json!({}), Unique::Key("d1".into())).await.unwrap();
    assert_eq!(h.store.wallet(OTHER).await.unwrap().unwrap().entity_id, Some(1));
    h.tg.clear();

    // Weak evidence for E#2 changes nothing and says nothing.
    assert_eq!(h.app.add_evidence(OTHER, 2, kind::COBUY_SLOTS, None, vec![], json!({}), Unique::Key("m".into())).await.unwrap(), None);
    assert!(h.tg.sent().is_empty());
    // Proof for E#2 as well: the wallet stays put and the admins are asked, once.
    assert_eq!(h.app.add_evidence(OTHER, 2, kind::DEPOSIT_PROOF, Some(100), vec![], json!({}), Unique::Key("d2".into())).await.unwrap(), None);
    h.app.rejudge(OTHER, 2).await.unwrap();
    assert_eq!(h.store.wallet(OTHER).await.unwrap().unwrap().entity_id, Some(1));
    let hints = h.tg.sent_to(ADMIN);
    assert_eq!(hints.len(), 1, "{hints:?}");
    assert!(hints[0].contains("/merge 2 1"), "{}", hints[0]);

    // The merge: wallets, evidence and follows move; the old entity is retired.
    h.store.set_follow(FRIEND, 2, FollowMode::Auto).await.unwrap();
    let reply = h.say(ADMIN, "/merge 2 1").await;
    assert!(reply[0].contains("1 wallet moved from E#2 into E#1"), "{}", reply[0]);
    let wallets: Vec<(String, Option<LinkedBy>)> = h.store.wallets_of(1).await.unwrap().into_iter().filter(|w| w.state == WalletState::Linked).map(|w| (w.address, w.linked_by)).collect();
    assert_eq!(wallets.len(), 3);
    assert!(wallets.contains(&(WALLET_2.to_string(), Some(LinkedBy::Admin))), "a second root becomes an ordinary linked wallet: {wallets:?}");
    assert_eq!(h.store.entity(2).await.unwrap().unwrap().status, EntityStatus::Retired);
    assert!(h.store.evidence_of_entity(2).await.unwrap().is_empty());
    assert_eq!(h.store.follows_of_entity(1).await.unwrap().iter().find(|f| f.account == FRIEND).unwrap().mode, FollowMode::Cards, "an existing follow is kept as it was");
    assert!(h.say(FRIEND, "/merge 1 2").await[0].contains("for admins"));
    assert!(h.say(ADMIN, "/merge 1 1").await[0].contains("two different entities"));

    // And a split: the wallet becomes the root of a new entity.
    let reply = h.say(ADMIN, &format!("/split {OTHER}")).await;
    assert!(reply[0].contains("<b>E#3 split from E#1</b>"), "{}", reply[0]);
    let wallet = h.store.wallet(OTHER).await.unwrap().unwrap();
    assert_eq!((wallet.entity_id, wallet.state, wallet.linked_by), (Some(3), WalletState::Linked, Some(LinkedBy::Root)));
    assert!(h.store.evidence_of_wallet(OTHER).await.unwrap().iter().filter(|e| e.entity_id == 1).all(|e| e.voided));
}

#[tokio::test]
async fn candidates_are_listed_and_a_quiet_entity_is_reported_lost_once() {
    let h = tracked().await;
    assert!(h.say(ADMIN, "/candidates").await[0].contains("No candidates"));
    h.app.add_evidence(OTHER, 1, kind::DIRECT_FUNDING, None, vec![], json!({}), Unique::Key("f".into())).await.unwrap();
    let list = h.say(ADMIN, "/candidates").await;
    assert!(list[0].contains("<b>Candidates (1)</b>") && list[0].contains("· 45 · E#1 whale"), "{}", list[0]);

    // Five quiet days, but a candidate is in sight: not lost.
    h.advance(6 * 86_400);
    assert_eq!(h.app.lost_check().await.unwrap(), 0);
    h.press(ADMIN, &format!("n:{OTHER}"), 1).await;
    h.tg.clear();
    assert_eq!(h.app.lost_check().await.unwrap(), 1);
    let notice = h.tg.last_to(FRIEND);
    assert!(notice.contains("quiet for 6 days"), "{notice}");
    assert_eq!(h.tg.last_buttons(FRIEND), vec![("Retire".to_string(), "x:1".to_string()), ("Keep watching".to_string(), "k:1".to_string())]);
    assert_eq!(h.app.lost_check().await.unwrap(), 0, "asked once, not every pass");
    h.press(ADMIN, "x:1", 1).await;
    assert_eq!(h.store.entity(1).await.unwrap().unwrap().status, EntityStatus::Retired);
    assert!(h.store.watched_wallets().await.unwrap().is_empty());
}
