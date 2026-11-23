//! The trader: transaction bytes, the gate, the screen, and the whole copy
//! path on paper and live against a fake venue.

mod common;

use std::sync::Arc;

use common::*;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use regnerus::chain::solana::query::MintFacts;
use regnerus::config::LIVE_ACK_PHRASE;
use regnerus::identity::Unique;
use regnerus::identity::score::kind;
use regnerus::model::*;
use regnerus::store::Store;
use regnerus::trade::gate::{Decision, ExitRules, Facts, copyability, decide, decide_exit, screen, sell_fraction};
use regnerus::trade::sign::{ASSOCIATED_TOKEN_PROGRAM, JUPITER_PROGRAM, Keypair, associated_token_address, decode_base64, encode_base64, parse_transaction, sign_transaction, vet_swap};
use regnerus::trade::venue::VenueError;
use serde_json::{Value, json};

const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
/// Lamports per raw token unit: 0.05 SOL buys about 1.67e12 units.
const PRICE: f64 = 0.00003;

// ---- transaction bytes ----

fn key(address: &str) -> [u8; 32] {
    bs58::decode(address).into_vec().unwrap().try_into().unwrap()
}

struct Ix {
    program: u8,
    accounts: Vec<u8>,
    data: Vec<u8>,
}

fn ix(program: u8, accounts: &[u8], data: &[u8]) -> Ix {
    Ix { program, accounts: accounts.to_vec(), data: data.to_vec() }
}

fn system_transfer(lamports: u64) -> Vec<u8> {
    let mut data = vec![2, 0, 0, 0];
    data.extend_from_slice(&lamports.to_le_bytes());
    data
}

/// Serializes an unsigned transaction: empty signature slots, then the message.
fn tx_bytes(v0: bool, signers: u8, keys: &[[u8; 32]], ixs: &[Ix]) -> Vec<u8> {
    let mut out = vec![signers];
    out.extend(std::iter::repeat_n(0u8, 64 * signers as usize));
    if v0 {
        out.push(0x80);
    }
    out.extend([signers, 0, 1, keys.len() as u8]);
    for k in keys {
        out.extend_from_slice(k);
    }
    out.extend([7u8; 32]); // recent blockhash
    out.push(ixs.len() as u8);
    for i in ixs {
        out.push(i.program);
        out.push(i.accounts.len() as u8);
        out.extend_from_slice(&i.accounts);
        out.push(i.data.len() as u8);
        out.extend_from_slice(&i.data);
    }
    if v0 {
        out.push(0); // no lookup tables
    }
    out
}

/// A swap shaped the way the venue builds one for `wallet`: set the compute
/// budget, wrap the SOL being spent, call Jupiter.
fn swap_tx(wallet: &[u8; 32], wrap_lamports: u64) -> Vec<u8> {
    let address = bs58::encode(wallet).into_string();
    let wsol = key(&associated_token_address(&address, WSOL, TOKEN_PROGRAM).unwrap());
    let keys = [*wallet, wsol, key(SYSTEM), key(TOKEN_PROGRAM), key(JUPITER_PROGRAM), key(COMPUTE_BUDGET)];
    let mut ixs = vec![ix(5, &[], &[2, 0x40, 0x0d, 3, 0])];
    if wrap_lamports > 0 {
        ixs.push(ix(2, &[0, 1], &system_transfer(wrap_lamports)));
        ixs.push(ix(3, &[1], &[17]));
    }
    ixs.push(ix(4, &[0, 1], &[1, 2, 3]));
    tx_bytes(true, 1, &keys, &ixs)
}

#[test]
fn a_transaction_is_parsed_signed_in_our_slot_and_verifies() {
    let ours = Keypair::generate().unwrap();
    let bytes = swap_tx(&ours.public_bytes(), 50_000_000);
    let parsed = parse_transaction(&bytes).unwrap();
    assert_eq!((parsed.versioned, parsed.required_signatures, parsed.static_keys.len(), parsed.instructions.len()), (true, 1, 6, 4));
    assert_eq!(parsed.fee_payer(), ours.address());

    let (signed, id) = sign_transaction(&bytes, &ours).unwrap();
    assert_eq!(signed.len(), bytes.len());
    assert_eq!(&signed[65..], &bytes[65..], "the message is untouched");
    let signature = Signature::from_bytes(signed[1..65].try_into().unwrap());
    VerifyingKey::from_bytes(&ours.public_bytes()).unwrap().verify(&bytes[65..], &signature).expect("the signature is over the message bytes");
    assert_eq!(id, bs58::encode(&signed[1..65]).into_string(), "the transaction id is the first signature");
    assert_eq!(decode_base64(&encode_base64(&signed)).unwrap(), signed);

    // Someone else's transaction is not signed.
    let theirs = Keypair::generate().unwrap();
    assert!(sign_transaction(&bytes, &theirs).unwrap_err().to_string().contains("is not a signer"));
    // With two signers, only our slot is written.
    let two = tx_bytes(false, 2, &[theirs.public_bytes(), ours.public_bytes(), key(SYSTEM)], &[ix(2, &[0, 1], &system_transfer(1))]);
    let (signed, _) = sign_transaction(&two, &ours).unwrap();
    assert!(signed[1..65].iter().all(|b| *b == 0), "the other signer's slot stays empty");
    assert!(signed[65..129].iter().any(|b| *b != 0));

    // Malformed bytes are errors, never a guess.
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(parse_transaction(&trailing).unwrap_err().to_string().contains("trailing"));
    assert!(parse_transaction(&bytes[..bytes.len() - 5]).is_err());
    assert!(parse_transaction(&[]).is_err());
    let mut v1 = bytes.clone();
    v1[65] = 0x81;
    assert!(parse_transaction(&v1).unwrap_err().to_string().contains("unsupported message version"));
}

#[test]
fn keys_are_read_in_the_forms_wallets_export_and_token_accounts_are_derived_off_curve() {
    let made = Keypair::generate().unwrap();
    let again = Keypair::from_secret(&made.secret_base58()).unwrap();
    assert_eq!(again.address(), made.address());
    let as_list: Vec<u8> = bs58::decode(made.secret_base58()).into_vec().unwrap();
    assert_eq!(Keypair::from_secret(&json!(as_list).to_string()).unwrap().address(), made.address(), "the JSON list form");
    assert_eq!(Keypair::from_secret(&bs58::encode(&as_list[..32]).into_string()).unwrap().address(), made.address(), "the bare seed");
    let mut mismatched = as_list.clone();
    mismatched[40] ^= 1;
    assert!(Keypair::from_secret(&bs58::encode(&mismatched).into_string()).is_err(), "halves that do not belong together");
    assert!(Keypair::from_secret("not a key").is_err());
    assert_ne!(Keypair::generate().unwrap().address(), made.address());

    let ata = associated_token_address(&made.address(), WSOL, TOKEN_PROGRAM).unwrap();
    assert_eq!(ata, associated_token_address(&made.address(), WSOL, TOKEN_PROGRAM).unwrap());
    assert_ne!(ata, associated_token_address(&made.address(), MEME_MINT, TOKEN_PROGRAM).unwrap());
    assert_ne!(ata, associated_token_address(&Keypair::generate().unwrap().address(), WSOL, TOKEN_PROGRAM).unwrap());
    // A derived address is not a public key: no key can sign for it.
    assert!(curve25519_dalek::edwards::CompressedEdwardsY(key(&ata)).decompress().is_none());
    assert!(curve25519_dalek::edwards::CompressedEdwardsY(made.public_bytes()).decompress().is_some());
}

#[test]
fn only_a_swap_built_for_this_wallet_alone_is_signed() {
    let ours = Keypair::generate().unwrap();
    let wallet = ours.address();
    let pk = ours.public_bytes();
    let vet = |bytes: &[u8], mint: &str, amount: u64| vet_swap(bytes, &wallet, mint, amount, &[]);
    let stranger = key(&addr(9));

    assert_eq!(vet(&swap_tx(&pk, 250_000_000), WSOL, 250_000_000), Ok(()), "wrapping exactly what was quoted");
    assert_eq!(vet(&swap_tx(&pk, 0), MEME_MINT, 5), Ok(()), "a sell wraps nothing");
    assert!(vet(&swap_tx(&pk, 250_000_000), WSOL, 100_000_000).unwrap_err().contains("would wrap 250000000 lamports"));
    assert!(vet(&swap_tx(&pk, 250_000_000), MEME_MINT, 5).unwrap_err().contains("would wrap"), "a sell has no SOL to wrap");

    let keys = [pk, stranger, key(SYSTEM), key(TOKEN_PROGRAM), key(JUPITER_PROGRAM), key(ASSOCIATED_TOKEN_PROGRAM)];
    let with = |ixs: &[Ix]| tx_bytes(true, 1, &keys, ixs);
    // Rent and tips to others are allowed up to a small ceiling; more is not.
    assert_eq!(vet(&with(&[ix(2, &[0, 1], &system_transfer(2_000_000)), ix(4, &[0], &[1])]), WSOL, 0), Ok(()));
    assert!(vet(&with(&[ix(2, &[0, 1], &system_transfer(1_000_000_000))]), WSOL, 0).unwrap_err().contains("to other accounts"));
    // A transfer someone else funds is not ours to limit.
    assert_eq!(vet(&with(&[ix(2, &[1, 0], &system_transfer(1_000_000_000))]), WSOL, 0), Ok(()));
    // Approvals and authority changes on token accounts.
    assert!(vet(&with(&[ix(3, &[1, 0], &[4, 1, 0, 0, 0, 0, 0, 0, 0])]), WSOL, 0).unwrap_err().contains("approve a delegate"));
    assert!(vet(&with(&[ix(3, &[1, 0], &[6, 2, 0])]), WSOL, 0).unwrap_err().contains("change the authority"));
    // A program that is not part of a swap.
    let odd = [pk, stranger, key(PUMP)];
    assert!(vet(&tx_bytes(true, 1, &odd, &[ix(2, &[0], &[1])]), WSOL, 0).unwrap_err().contains("not an allowed swap program"));
    assert_eq!(vet_swap(&tx_bytes(true, 1, &odd, &[ix(2, &[0], &[1])]), &wallet, WSOL, 0, &[PUMP.to_string()]), Ok(()), "unless it is allowed by name");
    // Someone else pays, or a second signature is wanted.
    assert!(vet(&tx_bytes(true, 1, &[stranger, pk, key(JUPITER_PROGRAM)], &[ix(2, &[0], &[1])]), WSOL, 0).unwrap_err().contains("fee payer"));
    assert!(vet(&tx_bytes(true, 2, &[pk, stranger, key(JUPITER_PROGRAM)], &[ix(2, &[0], &[1])]), WSOL, 0).unwrap_err().contains("2 signatures"));
    assert!(vet(&[1, 2, 3], WSOL, 0).unwrap_err().contains("unreadable"));
}

// ---- the gate, the screen and the exit rules ----

fn account() -> Account {
    Account { id: FRIEND, name: "friend".into(), is_admin: false, paused: false, created_at: 0 }
}

fn entity(copyable: bool, bait: bool) -> Entity {
    Entity { id: 1, label: "whale".into(), status: EntityStatus::Tracking, copyable, bait, created_by: ADMIN, created_at: 0 }
}

fn ready() -> Facts {
    Facts { follow: Some(FollowMode::Auto), wallet_trusted: true, source_lamports: Some(SOL), ..Facts::default() }
}

#[test]
fn the_gate_copies_only_when_every_condition_holds() {
    let settings = TradeSettings::default();
    let good = entity(true, false);
    assert_eq!(decide(&account(), &settings, &good, &ready()), Decision::Copy { lamports: 50_000_000, live: false });
    assert_eq!(decide(&account(), &settings, &good, &Facts { size_override: Some(SOL / 5), ..ready() }), Decision::Copy { lamports: SOL / 5, live: false }, "a size set for this entity wins");
    assert!(matches!(decide(&account(), &settings, &good, &Facts { size_override: Some(SOL), ..ready() }), Decision::Hold(_)), "but not over the daily budget");

    let hold = |entity: &Entity, settings: &TradeSettings, facts: Facts| match decide(&account(), settings, entity, &facts) {
        Decision::Hold(why) => why,
        other => panic!("expected a hold, got {other:?}"),
    };
    assert!(matches!(decide(&account(), &settings, &good, &Facts { follow: Some(FollowMode::Cards), ..ready() }), Decision::Skip(_)), "cards only: nothing to say");
    assert!(matches!(decide(&Account { paused: true, ..account() }, &settings, &good, &ready()), Decision::Skip(_)));
    assert!(hold(&good, &settings, Facts { killed: true, ..ready() }).contains("stopped by an admin"));
    assert!(hold(&entity(true, true), &settings, ready()).contains("bait"));
    assert!(hold(&entity(false, false), &settings, ready()).contains("has not yet shown"));
    assert!(hold(&good, &settings, Facts { wallet_trusted: false, ..ready() }).contains("linked by score only"));
    assert!(hold(&good, &settings, Facts { open_positions: 3, ..ready() }).contains("3 open positions"));
    assert!(hold(&good, &settings, Facts { spent_today: 480_000_000, ..ready() }).contains("budget is used up"));
    assert_eq!(decide(&account(), &settings, &good, &Facts { spent_today: 450_000_000, ..ready() }), Decision::Copy { lamports: 50_000_000, live: false }, "exactly at the cap still fits");

    let picky = TradeSettings { min_source_lamports: 2 * SOL, ..settings.clone() };
    assert!(matches!(decide(&account(), &picky, &good, &ready()), Decision::Skip(_)), "the entity's buy was too small");
    let needs_two = TradeSettings { require_consensus: true, ..settings.clone() };
    assert!(hold(&good, &needs_two, Facts { consensus: 1, ..ready() }).contains("1 of 2 entities"));
    assert!(matches!(decide(&account(), &needs_two, &good, &Facts { consensus: 2, ..ready() }), Decision::Copy { .. }));
    assert!(matches!(decide(&account(), &TradeSettings { size_lamports: 0, ..settings.clone() }, &good, &ready()), Decision::Skip(_)));

    // Live needs the user's choice, the server's switch and a wallet.
    let live = TradeSettings { live: true, ..settings.clone() };
    assert_eq!(decide(&account(), &live, &good, &ready()), Decision::Copy { lamports: 50_000_000, live: false }, "the server's switch is off: paper");
    assert!(hold(&good, &live, Facts { live_enabled: true, ..ready() }).contains("no trading wallet"));
    assert_eq!(decide(&account(), &live, &good, &Facts { live_enabled: true, has_wallet: true, ..ready() }), Decision::Copy { lamports: 50_000_000, live: true });
}

#[test]
fn the_screen_refuses_tokens_that_can_be_frozen_taxed_or_inflated() {
    let clean = MintFacts { exists: true, decimals: 6, ..MintFacts::default() };
    assert_eq!(screen(&clean, 0.4, None), Ok(()));
    assert_eq!(screen(&clean, -3.0, Some(2_900)), Ok(()));
    assert!(screen(&MintFacts::default(), 0.0, None).unwrap_err().contains("could not be read"));
    assert!(screen(&MintFacts { freeze_authority: Some("x".into()), ..clean.clone() }, 0.0, None).unwrap_err().contains("freeze"));
    assert!(screen(&MintFacts { mint_authority: Some("x".into()), ..clean.clone() }, 0.0, None).unwrap_err().contains("mint more supply"));
    assert!(screen(&MintFacts { extensions: vec!["metadataPointer".into(), "transferFeeConfig".into()], ..clean.clone() }, 0.0, None).unwrap_err().contains("taxes every transfer"));
    assert_eq!(screen(&MintFacts { extensions: vec!["metadataPointer".into(), "tokenMetadata".into()], ..clean.clone() }, 0.0, None), Ok(()), "harmless extensions pass");
    assert!(screen(&clean, -12.5, None).unwrap_err().contains("move the price by 12.5%"));
    assert!(screen(&clean, 0.0, Some(4_200)).unwrap_err().contains("42% of the supply"));
}

#[test]
fn exits_follow_the_rules_and_an_unvalued_position_is_never_stuck() {
    let rules = ExitRules { tp_pct: Some(100), sl_pct: Some(30), max_hold_min: Some(60) };
    assert_eq!(decide_exit(&rules, 100, Some(199), 0, 10), None);
    assert_eq!(decide_exit(&rules, 100, Some(200), 0, 10), Some("take_profit"));
    assert_eq!(decide_exit(&rules, 100, Some(70), 0, 10), Some("stop_loss"));
    assert_eq!(decide_exit(&rules, 100, Some(71), 0, 10), None);
    assert_eq!(decide_exit(&rules, 100, None, 0, 3_599), None, "no price, no price exit");
    assert_eq!(decide_exit(&rules, 100, None, 0, 3_600), Some("max_hold"));
    assert_eq!(decide_exit(&ExitRules { tp_pct: None, sl_pct: None, max_hold_min: None }, 100, Some(1), 0, 999_999), None);

    assert_eq!(sell_fraction(50, 100), 0.5);
    assert_eq!(sell_fraction(96, 100), 1.0, "nearly everything is everything");
    assert_eq!(sell_fraction(500, 100), 1.0, "more than was on record: everything");
    assert_eq!(sell_fraction(0, 100), 0.0);
}

// ---- the copy path ----

fn clean_mint(h: &Harness, mint: &str) {
    h.rpc.raw.lock().insert(
        format!("getMultipleAccounts:{}", json!([mint])),
        json!({ "value": [{ "data": { "program": "spl-token", "parsed": { "info": { "decimals": 6, "mintAuthority": null, "freezeAuthority": null } } } }] }),
    );
}

/// E#1 is tracked and copyable; the friend follows it on auto.
async fn trading(extra: &[(&str, &str)]) -> (Harness, Arc<FakeVenue>) {
    let mut env = vec![("COBUY_TXS", "0")];
    env.extend_from_slice(extra);
    let h = harness_with(&env);
    let venue = Arc::new(FakeVenue::default());
    venue.price(MEME_MINT, PRICE);
    h.app.set_venue(venue.clone());
    clean_mint(&h, MEME_MINT);
    h.say(ADMIN, &format!("/track {WALLET} whale")).await;
    h.say(ADMIN, "/copyable 1 on").await;
    h.store.upsert_account(FRIEND, "friend", false, T0).await.unwrap();
    h.store.set_follow(FRIEND, 1, FollowMode::Auto).await.unwrap();
    h.advance(30);
    h.work().await;
    h.tg.clear();
    (h, venue)
}

/// The entity sells `sold` raw units of the fixture's token.
fn entity_sell(signature: &str, sold: u64, at: i64) -> Value {
    let mut tx = as_wallet("jup_sell", WALLET, signature, at);
    tx["meta"]["preTokenBalances"][0]["uiTokenAmount"]["amount"] = json!(sold.to_string());
    tx
}

async fn positions(h: &Harness, account: AccountId) -> Vec<Position> {
    let mut rows = h.store.positions_of_account(account, 0).await.unwrap();
    rows.reverse();
    rows
}

const BOUGHT: u64 = 3_500_000_000_000;

#[tokio::test]
async fn an_entity_buy_is_copied_on_paper_and_its_sells_are_mirrored_by_fraction() {
    let (h, venue) = trading(&[]).await;
    h.see(WALLET, &fixture("pump_buy")).await;

    // The follower on auto has a paper position, filled at the quote.
    let mine = positions(&h, FRIEND).await;
    assert_eq!(mine.len(), 1);
    let expected = (50_000_000f64 / PRICE) as u64;
    assert_eq!((mine[0].mode, mine[0].status, mine[0].entry_lamports, mine[0].tokens_raw, mine[0].tokens_left), (PositionMode::Paper, PositionStatus::Open, 50_000_000, expected, expected));
    let card = h.tg.sent_to(FRIEND).into_iter().find(|t| t.contains("Copied")).expect("told");
    assert!(card.contains("Copied (paper)") && card.contains("0.05 SOL") && card.contains("price impact 0.10%"), "{card}");
    // The bot's own shadow copy of the entity, at its own size.
    let shadow = positions(&h, SHADOW_ACCOUNT).await;
    assert_eq!((shadow.len(), shadow[0].mode, shadow[0].entry_lamports), (1, PositionMode::Shadow, 100_000_000));
    // The admin follows with cards: a card with a Copy button, and no position.
    assert!(positions(&h, ADMIN).await.is_empty());
    assert!(h.tg.last_buttons(ADMIN).contains(&("Copy".to_string(), "b:1".to_string())), "{:?}", h.tg.last_buttons(ADMIN));
    // Paper asks for quotes, never for a transaction.
    assert!(venue.orders.lock().iter().all(|o| o.taker.is_none()));
    assert!(venue.executed.lock().is_empty());

    // A second fill of the same buy, seconds later, is the same signal: no second copy.
    h.advance(5);
    h.see(WALLET, &as_wallet("pump_buy", WALLET, "sigSecondFill", T0 + 5)).await;
    assert_eq!(positions(&h, FRIEND).await.len(), 1);

    // The entity sells half of what it holds: the copy sells half of what it holds.
    venue.price(MEME_MINT, PRICE * 1.5);
    h.tg.clear();
    h.advance(120);
    h.see(WALLET, &entity_sell("sigSellHalf", BOUGHT, T0 + 125)).await;
    let half = &positions(&h, FRIEND).await[0];
    assert_eq!((half.status, half.tokens_left), (PositionStatus::Open, expected - expected / 2));
    assert!(h.inbox(FRIEND).contains("The entity sold · part of #"), "{}", h.inbox(FRIEND));
    // It sells the rest: the copy closes, and the result is reported.
    h.advance(60);
    h.see(WALLET, &entity_sell("sigSellRest", BOUGHT, T0 + 185)).await;
    let done = &positions(&h, FRIEND).await[0];
    assert_eq!((done.status, done.tokens_left, done.exit_reason.as_deref()), (PositionStatus::Closed, 0, Some("source_exit")));
    assert!((done.pnl_lamports() - 25_000_000).abs() < 100, "sold everything at 1.5 times the entry: {}", done.pnl_lamports());
    assert!(h.tg.last_to(FRIEND).contains("The entity sold") && h.tg.last_to(FRIEND).contains("(+50.0%)"), "{}", h.tg.last_to(FRIEND));
    assert_eq!(positions(&h, SHADOW_ACCOUNT).await[0].status, PositionStatus::Closed, "the shadow copy follows the same exits");

    let listed = h.say(FRIEND, "/pnl").await;
    assert!(listed[0].contains("paper: 1 closed · 1 won"), "{}", listed[0]);
    assert!(h.say(FRIEND, "/positions").await[0].contains("No open positions"));
}

#[tokio::test]
async fn a_hold_is_explained_and_caps_the_stop_switch_and_standing_are_respected() {
    let (h, venue) = trading(&[]).await;
    venue.price(T22_MINT, PRICE);
    clean_mint(&h, T22_MINT);
    h.say(FRIEND, "/set maxopen 1").await;
    h.see(WALLET, &fixture("pump_buy")).await;
    assert_eq!(positions(&h, FRIEND).await.len(), 1);

    // A second token while the first is open: over the limit.
    h.tg.clear();
    h.see(WALLET, &fixture("t22_buy")).await;
    assert_eq!(positions(&h, FRIEND).await.len(), 1);
    assert!(h.inbox(FRIEND).contains("Not copied: you already have 1 open positions (your limit is 1)."), "{}", h.inbox(FRIEND));
    assert_eq!(positions(&h, SHADOW_ACCOUNT).await.len(), 2, "the shadow copy has no caps");

    // The admin's stop switch.
    h.say(FRIEND, "/set maxopen off").await;
    assert!(h.say(FRIEND, "/kill").await[0].contains("for admins"));
    assert!(h.say(ADMIN, "/kill").await[0].contains("Stopped"));
    h.tg.clear();
    h.advance(100);
    h.see(WALLET, &as_wallet("t22_buy", WALLET, "sigAfterKill", T0 + 200)).await;
    assert!(h.inbox(FRIEND).contains("stopped by an admin"), "{}", h.inbox(FRIEND));
    assert!(h.say(ADMIN, "/health").await[0].contains("Trader: stopped by /kill · venue fake-venue"));
    h.say(ADMIN, "/unkill").await;

    // An entity that has not earned it is not copied, whatever the follow says.
    h.say(ADMIN, "/copyable 1 off").await;
    h.tg.clear();
    h.advance(100);
    h.see(WALLET, &as_wallet("t22_buy", WALLET, "sigNotCopyable", T0 + 400)).await;
    assert!(h.inbox(FRIEND).contains("has not yet shown that a delayed copy of it makes money"), "{}", h.inbox(FRIEND));
    assert_eq!(positions(&h, FRIEND).await.len(), 1);
}

#[tokio::test]
async fn a_buy_found_late_is_a_card_and_not_a_copy_and_a_reason_is_given_once() {
    let (h, _venue) = trading(&[]).await;
    // Found ten minutes after it happened, as after an outage.
    h.advance(600);
    h.see(WALLET, &fixture("pump_buy")).await;
    assert!(h.inbox(FRIEND).contains("<b>BUY</b> · <b>E#1 whale</b>"), "the card still goes out");
    assert!(h.inbox(FRIEND).contains("Not copied: the buy was found too late to copy at its price."), "{}", h.inbox(FRIEND));
    assert!(h.store.open_positions().await.unwrap().is_empty(), "no copy, and no shadow copy at a price that is gone");
    // The same reason for the same entity is not repeated on the next late buy.
    h.tg.clear();
    h.see(WALLET, &as_wallet("t22_buy", WALLET, "sigAlsoLate", T0 + 20)).await;
    assert!(h.inbox(FRIEND).contains("<b>BUY</b>") && !h.inbox(FRIEND).contains("Not copied"), "{}", h.inbox(FRIEND));
}

#[tokio::test]
async fn a_wallet_linked_by_score_sends_cards_but_is_not_copied_until_calibration_is_full() {
    let (h, _venue) = trading(&[]).await;
    for (kind_name, key) in [(kind::DIRECT_FUNDING, "f"), (kind::COBUY_SLOTS, "a"), (kind::COBUY_SLOTS, "b"), (kind::COBUY_SLOTS, "c")] {
        h.app.add_evidence(WALLET_2, 1, kind_name, None, vec![], json!({}), Unique::Key(key.into())).await.unwrap();
    }
    assert_eq!(h.store.wallet(WALLET_2).await.unwrap().unwrap().linked_by, Some(LinkedBy::Score));
    h.tg.clear();
    h.see(WALLET_2, &as_wallet("pump_buy", WALLET_2, "sigScoreLinkedBuy", T0 + 40)).await;
    assert!(h.inbox(FRIEND).contains("<b>BUY</b> · <b>E#1 whale</b>"), "the card is sent");
    assert!(h.inbox(FRIEND).contains("linked by score only"), "{}", h.inbox(FRIEND));
    assert!(positions(&h, FRIEND).await.is_empty());

    // With thirty proofs on the table, the same kind of wallet is trusted.
    h.store.set_state("proofs", "30").await.unwrap();
    h.advance(100);
    h.see(WALLET_2, &as_wallet("pump_buy", WALLET_2, "sigScoreLinkedBuy2", T0 + 140)).await;
    assert_eq!(positions(&h, FRIEND).await.len(), 1);
}

#[tokio::test]
async fn a_token_that_fails_the_screen_becomes_a_card_that_says_why() {
    let (h, venue) = trading(&[]).await;
    h.rpc.raw.lock().insert(
        format!("getMultipleAccounts:{}", json!([MEME_MINT])),
        json!({ "value": [{ "data": { "program": "spl-token", "parsed": { "info": { "decimals": 6, "mintAuthority": null, "freezeAuthority": OTHER } } } }] }),
    );
    h.see(WALLET, &fixture("pump_buy")).await;
    let mine = positions(&h, FRIEND).await;
    assert_eq!((mine.len(), mine[0].status), (1, PositionStatus::Failed));
    assert!(h.inbox(FRIEND).contains("Not copied (paper)") && h.inbox(FRIEND).contains("freeze authority is set"), "{}", h.inbox(FRIEND));
    assert!(h.store.open_positions().await.unwrap().is_empty());

    // A size that would move the price too far.
    clean_mint(&h, MEME_MINT);
    *venue.impact_pct.lock() = -14.0;
    h.advance(100);
    h.see(WALLET, &as_wallet("pump_buy", WALLET, "sigThin", T0 + 100)).await;
    assert!(positions(&h, FRIEND).await[1].note.contains("move the price by 14.0%"));
    // A fresh holder map showing one cluster with most of the supply.
    *venue.impact_pct.lock() = 0.2;
    h.store.put_doc("map", MEME_MINT, &json!({ "at": T0 + 150, "largest_bps": 6_000 }), 0).await.unwrap();
    h.advance(100);
    h.see(WALLET, &as_wallet("pump_buy", WALLET, "sigClustered", T0 + 200)).await;
    assert!(positions(&h, FRIEND).await[2].note.contains("60% of the supply"));
    // A token with no route at all.
    h.store.delete_doc("map", MEME_MINT).await.unwrap();
    venue.no_route(MEME_MINT);
    h.advance(100);
    h.see(WALLET, &as_wallet("pump_buy", WALLET, "sigNoRoute", T0 + 300)).await;
    assert!(positions(&h, FRIEND).await[3].note.contains("no route"));
}

#[tokio::test]
async fn the_monitor_takes_profit_and_stops_losses_only_on_two_readings_and_times_out_at_once() {
    let (h, venue) = trading(&[("SHADOW_SOL", "0")]).await;
    h.see(WALLET, &fixture("pump_buy")).await;
    let id = positions(&h, FRIEND).await[0].id;
    h.tg.clear();

    // One reading at double is not enough.
    venue.price(MEME_MINT, PRICE * 2.1);
    assert_eq!(h.app.monitor_positions().await, 1);
    assert_eq!(h.store.position(id).await.unwrap().unwrap().status, PositionStatus::Open);
    // A reading back under the line forgets the first.
    venue.price(MEME_MINT, PRICE * 1.5);
    h.app.monitor_positions().await;
    venue.price(MEME_MINT, PRICE * 2.1);
    h.app.monitor_positions().await;
    assert_eq!(h.store.position(id).await.unwrap().unwrap().status, PositionStatus::Open, "the two readings must be in a row");
    h.app.monitor_positions().await;
    let closed = h.store.position(id).await.unwrap().unwrap();
    assert_eq!((closed.status, closed.exit_reason.as_deref()), (PositionStatus::Closed, Some("take_profit")));
    assert!(h.tg.last_to(FRIEND).contains("Take profit") && h.tg.last_to(FRIEND).contains("+110.0%"), "{}", h.tg.last_to(FRIEND));

    // Stop loss, the same way.
    venue.price(MEME_MINT, PRICE);
    h.advance(100);
    h.see(WALLET, &as_wallet("pump_buy", WALLET, "sigBuyTwo", T0 + 100)).await;
    let id = positions(&h, FRIEND).await[1].id;
    venue.price(MEME_MINT, PRICE * 0.6);
    h.app.monitor_positions().await;
    h.app.monitor_positions().await;
    assert_eq!(h.store.position(id).await.unwrap().unwrap().exit_reason.as_deref(), Some("stop_loss"));

    // Max hold needs no price and no second reading. On paper, a token that
    // cannot be sold when time is up is counted as a total loss.
    venue.price(MEME_MINT, PRICE);
    h.advance(100);
    h.see(WALLET, &as_wallet("pump_buy", WALLET, "sigBuyThree", T0 + 200)).await;
    let id = positions(&h, FRIEND).await[2].id;
    venue.no_route(MEME_MINT);
    h.advance(119 * 60);
    h.app.monitor_positions().await;
    assert_eq!(h.store.position(id).await.unwrap().unwrap().status, PositionStatus::Open);
    h.advance(60);
    h.app.monitor_positions().await;
    let dead = h.store.position(id).await.unwrap().unwrap();
    assert_eq!((dead.status, dead.exit_reason.as_deref(), dead.exit_lamports), (PositionStatus::Closed, Some("max_hold"), 0));
    assert_eq!(dead.pnl_lamports(), -50_000_000);
}

#[tokio::test]
async fn the_copy_button_and_close_are_the_users_own_call_and_a_second_tap_does_nothing() {
    let (h, _venue) = trading(&[("SHADOW_SOL", "0")]).await;
    h.say(ADMIN, "/copyable 1 off").await;
    h.see(WALLET, &fixture("pump_buy")).await;
    assert!(h.store.open_positions().await.unwrap().is_empty(), "nobody is copied automatically");

    // The admin taps Copy under the buy card, twice.
    h.press(ADMIN, "b:1", 5).await;
    h.press(ADMIN, "b:1", 5).await;
    assert_eq!(h.tg.toasts(), vec!["Copying, on paper.", "Copying, on paper."]);
    let mine = positions(&h, ADMIN).await;
    assert_eq!((mine.len(), mine[0].status, mine[0].mode), (1, PositionStatus::Open, PositionMode::Paper));
    assert_eq!(h.tg.sent_to(ADMIN).iter().filter(|t| t.contains("Copied (paper)")).count(), 1, "the second tap opened nothing");
    assert!(h.say(ADMIN, "/positions").await[0].contains(&format!("<code>/close {}</code>", mine[0].id)));

    // Closing: only the owner can, and only once.
    assert!(h.say(FRIEND, &format!("/close {}", mine[0].id)).await[0].contains("no position with that number"));
    assert!(h.say(ADMIN, &format!("/close {}", mine[0].id)).await[0].contains("Closing"));
    settle().await;
    assert_eq!(h.store.position(mine[0].id).await.unwrap().unwrap().exit_reason.as_deref(), Some("manual"));
    assert!(h.say(ADMIN, &format!("/close {}", mine[0].id)).await[0].contains("is closed"));
    h.press(ADMIN, "b:999", 5).await;
    assert_eq!(h.tg.toasts().last().unwrap(), "That signal is gone.");
}

// ---- live ----

const LIVE: &[(&str, &str)] = &[("EXECUTION_LIVE_MODE", "true"), ("EXECUTION_LIVE_ACK", LIVE_ACK_PHRASE), ("SHADOW_SOL", "0")];

/// The friend has a funded trading wallet whose key the provider holds, and is set to live.
async fn live(extra: &[(&str, &str)]) -> (Harness, Arc<FakeVenue>, Keypair) {
    let mut env = LIVE.to_vec();
    env.extend_from_slice(extra);
    let (h, venue) = trading(&env).await;
    let wallet = Keypair::generate().unwrap();
    let keys = Arc::new(FakeKeys::default());
    keys.secrets.lock().insert("WALLET_FRIEND".into(), wallet.secret_base58());
    h.app.set_keys(keys);
    assert!(h.say(FRIEND, "/usewallet WALLET_FRIEND").await[0].contains(&wallet.address()));
    assert!(h.say(FRIEND, "/live on").await[0].contains("LIVE"));
    h.rpc.raw.lock().insert(format!("getBalance:{}", wallet.address()), json!({ "value": SOL }));
    let public = wallet.public_bytes();
    *venue.build.lock() = Some(Box::new(move |request| encode_base64(&swap_tx(&public, if request.input_mint == WSOL { request.amount } else { 0 }))));
    h.tg.clear();
    (h, venue, wallet)
}

#[tokio::test]
async fn a_live_copy_is_vetted_signed_and_executed_and_its_exit_too() {
    let (h, venue, wallet) = live(&[]).await;
    h.see(WALLET, &fixture("pump_buy")).await;

    let mine = positions(&h, FRIEND).await;
    assert_eq!((mine.len(), mine[0].mode, mine[0].status), (1, PositionMode::Live, PositionStatus::Open));
    assert_eq!(mine[0].open_signature.as_deref(), Some("sigFill1"));
    assert!(h.inbox(FRIEND).contains("Copied (LIVE)"), "{}", h.inbox(FRIEND));
    // The order named our wallet as the taker, and what was executed carries our signature.
    let order = venue.orders.lock()[0].clone();
    assert_eq!((order.taker.as_deref(), order.amount, order.output_mint.as_str()), (Some(wallet.address().as_str()), 50_000_000, MEME_MINT));
    let (signed, request_id) = venue.executed.lock()[0].clone();
    assert_eq!(request_id, "req1");
    let bytes = decode_base64(&signed).unwrap();
    let signature = Signature::from_bytes(bytes[1..65].try_into().unwrap());
    VerifyingKey::from_bytes(&wallet.public_bytes()).unwrap().verify(&bytes[65..], &signature).expect("signed by the trading wallet");
    // The executor lease is held by this process.
    assert!(!h.store.acquire_lease("executor", "someone-else", 60, h.app.now()).await.unwrap());

    // The entity sells everything: a real sell goes out, signed the same way.
    h.tg.clear();
    h.advance(200);
    h.see(WALLET, &entity_sell("sigLiveExit", BOUGHT, T0 + 200)).await;
    let closed = &positions(&h, FRIEND).await[0];
    assert_eq!((closed.status, closed.exit_reason.as_deref(), closed.exit_lamports), (PositionStatus::Closed, Some("source_exit"), 49_999_999));
    assert_eq!(venue.executed.lock().len(), 2);
    let sell = venue.orders.lock().iter().rfind(|o| o.taker.is_some()).cloned().unwrap();
    assert_eq!((sell.input_mint.as_str(), sell.amount), (MEME_MINT, closed.tokens_raw));
    assert!(h.say(FRIEND, "/pnl").await[0].contains("LIVE: 1 closed"));
    // Nothing the bot said or logged to chat carries the key.
    assert!(h.tg.sent().iter().all(|(_, text)| !text.contains(&wallet.secret_base58())));
}

#[tokio::test]
async fn nothing_is_signed_when_the_transaction_the_wallet_or_the_server_is_not_right() {
    let (h, venue, wallet) = live(&[]).await;
    let public = wallet.public_bytes();
    let note = |positions: Vec<Position>| positions.last().map(|p| (p.status, p.note.clone())).unwrap();
    let mut n = 0;
    let mut buy = |h: &Harness| {
        n += 1;
        h.advance(100);
        as_wallet("pump_buy", WALLET, &format!("sigTry{n}"), T0 + 100 * n as i64)
    };

    // The venue's transaction would also send a whole SOL to a stranger.
    *venue.build.lock() = Some(Box::new(move |_| {
        let keys = [public, key(&addr(9)), key(SYSTEM), key(JUPITER_PROGRAM)];
        encode_base64(&tx_bytes(true, 1, &keys, &[ix(2, &[0, 1], &system_transfer(SOL)), ix(3, &[0], &[1])]))
    }));
    h.see(WALLET, &buy(&h)).await;
    let (status, why) = note(positions(&h, FRIEND).await);
    assert_eq!(status, PositionStatus::Failed);
    assert!(why.contains("refused to sign") && why.contains("to other accounts"), "{why}");
    assert!(venue.executed.lock().is_empty(), "a refused transaction is never sent");

    // A sponsored transaction needs a second signer: not ours to sign.
    *venue.build.lock() = Some(Box::new(move |request| encode_base64(&swap_tx(&public, request.amount))));
    *venue.gasless.lock() = true;
    h.see(WALLET, &buy(&h)).await;
    assert!(note(positions(&h, FRIEND).await).1.contains("second signer"));
    *venue.gasless.lock() = false;

    // The wallet cannot cover the size and the reserve.
    h.rpc.raw.lock().insert(format!("getBalance:{}", wallet.address()), json!({ "value": 60_000_000 }));
    h.see(WALLET, &buy(&h)).await;
    assert!(note(positions(&h, FRIEND).await).1.contains("holds 0.06 SOL; this copy needs 0.07 with the reserve"));
    h.rpc.raw.lock().insert(format!("getBalance:{}", wallet.address()), json!({ "value": SOL }));

    // Another running copy of the bot holds the lease.
    h.store.set_state("lease:executor", &format!("other|{}", h.app.now() + 10_000)).await.unwrap();
    h.see(WALLET, &buy(&h)).await;
    assert!(note(positions(&h, FRIEND).await).1.contains("holds the executor lease"));
    h.store.set_state("lease:executor", "other|0").await.unwrap();

    // The secret behind the wallet is someone else's key.
    let keys = Arc::new(FakeKeys::default());
    keys.secrets.lock().insert("WALLET_FRIEND".into(), Keypair::generate().unwrap().secret_base58());
    h.app.set_keys(keys);
    h.see(WALLET, &buy(&h)).await;
    assert!(note(positions(&h, FRIEND).await).1.contains("does not belong to the trading wallet"));
    assert!(venue.executed.lock().is_empty());
    assert_eq!(positions(&h, FRIEND).await.len(), 5, "each attempt is on record");
}

#[tokio::test]
async fn an_order_that_never_reports_back_is_settled_from_the_wallets_balance() {
    let (h, venue, wallet) = live(&[]).await;
    *venue.fail_execute.lock() = Some(VenueError::new("network", "connection reset"));
    h.see(WALLET, &fixture("pump_buy")).await;
    let id = positions(&h, FRIEND).await[0].id;
    assert_eq!(h.store.position(id).await.unwrap().unwrap().status, PositionStatus::Unconfirmed);
    assert!(h.inbox(FRIEND).contains("Copy sent (LIVE)"), "{}", h.inbox(FRIEND));

    // The tokens are in the wallet: it landed.
    let balance_key = format!("getTokenAccountsByOwner:{}", wallet.address());
    h.rpc.raw.lock().insert(balance_key.clone(), json!({ "value": [{ "account": { "data": { "parsed": { "info": { "tokenAmount": { "amount": "1600000000000" } } } } } }] }));
    h.app.monitor_positions().await;
    let landed = h.store.position(id).await.unwrap().unwrap();
    assert_eq!((landed.status, landed.tokens_left), (PositionStatus::Open, 1_600_000_000_000));

    // Another one that never landed: nothing in the wallet, and after a grace period it is written off.
    h.advance(100);
    h.see(WALLET, &as_wallet("pump_buy", WALLET, "sigLost", T0 + 100)).await;
    let lost = positions(&h, FRIEND).await[1].id;
    h.rpc.raw.lock().insert(balance_key, json!({ "value": [] }));
    h.app.monitor_positions().await;
    assert_eq!(h.store.position(lost).await.unwrap().unwrap().status, PositionStatus::Unconfirmed, "too soon to give up");
    h.advance(200);
    h.app.monitor_positions().await;
    let gone = h.store.position(lost).await.unwrap().unwrap();
    assert_eq!((gone.status, gone.note.as_str()), (PositionStatus::Failed, "the order never landed"));
}

#[tokio::test]
async fn live_stays_on_paper_without_the_servers_switch_and_wallets_are_never_made_unsafely() {
    // The server's switch is off: a user set to live still trades on paper.
    let (h, _venue) = trading(&[("SHADOW_SOL", "0")]).await;
    let keys = Arc::new(FakeKeys::default());
    h.app.set_keys(keys.clone());
    assert!(h.say(FRIEND, "/live on").await[0].contains("the server's live switch is off"));
    h.see(WALLET, &fixture("pump_buy")).await;
    assert_eq!(positions(&h, FRIEND).await[0].mode, PositionMode::Paper);

    // A provider that cannot keep a key makes none.
    assert!(h.say(FRIEND, "/wallets").await[0].contains("You have no trading wallet"));
    assert!(h.say(FRIEND, "/newwallet").await[0].contains("will not make one"));
    assert!(h.store.doc("twallet", &FRIEND.to_string()).await.unwrap().is_none());
    assert!(h.say(FRIEND, "/usewallet nope").await[0].contains("Usage"));
    assert!(h.say(FRIEND, "/usewallet MISSING_ONE").await[0].contains("Could not read that secret"));

    // One that can: the key goes to the provider first, and is never shown.
    let storing = Arc::new(FakeKeys { can_store: true, ..FakeKeys::default() });
    h.app.set_keys(storing.clone());
    let made = h.say(FRIEND, "/newwallet").await;
    let stored = storing.secrets.lock().get(&format!("REGNERUS_WALLET_{FRIEND}")).cloned().expect("stored under the account's name");
    let address = Keypair::from_secret(&stored).unwrap().address();
    assert!(made[0].contains(&address) && !made[0].contains(&stored), "{}", made[0]);
    assert!(h.say(FRIEND, "/newwallet").await[0].contains("already have a trading wallet"));
    h.rpc.raw.lock().insert(format!("getBalance:{address}"), json!({ "value": 1_500_000_000u64 }));
    let shown = h.say(FRIEND, "/wallets").await;
    assert!(shown[0].contains(&address) && shown[0].contains("1.5 SOL") && !shown[0].contains(&stored), "{}", shown[0]);
    // Nobody else can claim the same secret.
    assert!(h.say(ADMIN, &format!("/usewallet REGNERUS_WALLET_{FRIEND}")).await[0].contains("already someone else's"));
}

// ---- settings, copyability, bait ----

#[tokio::test]
async fn settings_are_per_user_and_only_take_values_that_make_sense() {
    let (h, _venue) = trading(&[]).await;
    let shown = h.say(FRIEND, "/settings").await;
    for part in ["Mode: paper", "Size per copy: 0.05 SOL", "Take profit: 100% · stop loss: 30% · max hold: 120 min", "Open positions limit: 3", "Slippage: auto", "Trading wallet: none"] {
        assert!(shown[0].contains(part), "missing {part:?} in:\n{}", shown[0]);
    }
    for (command, check) in [
        ("/set size 0.2", "Size per copy: 0.2 SOL"),
        ("/set tp off", "Take profit: off"),
        ("/set sl 45", "stop loss: 45%"),
        ("/set hold 30", "max hold: 30 min"),
        ("/set daily 2", "daily budget: 2 SOL"),
        ("/set slippage 250", "Slippage: 250 bps"),
        ("/set consensus on", "required to copy: yes"),
        ("/set window 15", "within 15 min"),
    ] {
        let reply = h.say(FRIEND, command).await;
        assert!(reply[0].contains(check), "{command}: {}", reply[0]);
    }
    for bad in ["/set size lots", "/set sl 400", "/set slippage 99999", "/set entities 1", "/set colour blue", "/set size", "/set consensus maybe"] {
        assert!(h.say(FRIEND, bad).await[0].contains("Usage"), "{bad}");
    }
    let mine = h.app.trade_settings(FRIEND).await;
    assert_eq!((mine.size_lamports, mine.tp_pct, mine.sl_pct, mine.slippage_bps, mine.require_consensus), (200_000_000, None, Some(45), Some(250), true));
    assert_eq!(h.app.trade_settings(ADMIN).await, TradeSettings::default(), "one user's settings are not another's");
}

fn closed_shadow(id: i64, pnl: i64, hold: i64) -> Position {
    Position {
        id,
        account: SHADOW_ACCOUNT,
        entity_id: 1,
        signal_id: id,
        mint: MEME_MINT.into(),
        decimals: 6,
        mode: PositionMode::Shadow,
        status: PositionStatus::Closed,
        entry_lamports: 100_000_000,
        tokens_raw: 1,
        tokens_left: 0,
        exit_lamports: (100_000_000 + pnl) as u64,
        opened_at: 1_000,
        closed_at: Some(1_000 + hold),
        exit_reason: Some("source_exit".into()),
        open_signature: None,
        note: String::new(),
    }
}

#[tokio::test]
async fn an_entity_earns_copyable_from_its_paper_copies_and_loses_it_after_two_bad_checks() {
    // Measured from shadow positions and the entity's own trades.
    let few: Vec<Position> = (1..=9).map(|i| closed_shadow(i, 5_000_000, 300)).collect();
    assert_eq!(copyability(&few, &[], false).failures(), vec!["9 of 10 paper trades".to_string()]);
    let fast: Vec<Position> = (1..=10).map(|i| closed_shadow(i, 5_000_000, 20)).collect();
    assert!(copyability(&fast, &[], false).failures()[0].contains("median hold 20s"));
    let losing: Vec<Position> = (1..=10).map(|i| closed_shadow(i, -5_000_000, 300)).collect();
    assert!(copyability(&losing, &[], true).failures().join(";").contains("not positive") && copyability(&losing, &[], true).failures().join(";").contains("bait"));
    let good: Vec<Position> = (1..=10).map(|i| closed_shadow(i, 5_000_000, 300)).collect();
    assert!(copyability(&good, &[], false).failures().is_empty());

    let (h, _venue) = trading(&[]).await;
    h.say(ADMIN, "/copyable 1 auto").await;
    assert!(!h.store.entity(1).await.unwrap().unwrap().copyable, "back on the paper results, which are empty");
    assert!(h.say(FRIEND, "/paper").await[0].contains("0 of 10 paper trades"));
    for position in &good {
        h.store.insert_position(position).await.unwrap();
    }
    h.tg.clear();
    h.app.copyability_pass().await.unwrap();
    assert!(h.store.entity(1).await.unwrap().unwrap().copyable);
    assert!(h.inbox(FRIEND).contains("is now copyable"), "{}", h.inbox(FRIEND));
    assert!(h.say(FRIEND, "/paper").await[0].contains("10 closed · +0.05 SOL · median hold 5m") && h.say(FRIEND, "/paper").await[0].contains("copyable"));

    // Its results turn: one bad check is forgiven, the second demotes it.
    for (i, position) in good.iter().enumerate() {
        let id = h.store.positions_of_entity(1, PositionMode::Shadow).await.unwrap()[i].id;
        h.store.update_position(&Position { id, exit_lamports: 80_000_000, ..position.clone() }).await.unwrap();
    }
    h.tg.clear();
    h.app.copyability_pass().await.unwrap();
    assert!(h.store.entity(1).await.unwrap().unwrap().copyable, "once is forgiven");
    h.app.copyability_pass().await.unwrap();
    assert!(!h.store.entity(1).await.unwrap().unwrap().copyable);
    assert!(h.inbox(FRIEND).contains("no longer copyable: paper result is not positive"), "{}", h.inbox(FRIEND));
    // An admin's pin is not overridden by the results.
    h.say(ADMIN, "/copyable 1 on").await;
    h.app.copyability_pass().await.unwrap();
    assert!(h.store.entity(1).await.unwrap().unwrap().copyable);
}

#[tokio::test]
async fn a_side_wallet_selling_into_the_entitys_buy_flags_it_as_bait() {
    let (h, _venue) = trading(&[]).await;
    // A wallet tied to the entity by evidence, but not linked: a candidate.
    h.app.add_evidence(WALLET_2, 1, kind::DIRECT_FUNDING, None, vec![], json!({}), Unique::Key("f".into())).await.unwrap();
    h.see(WALLET, &fixture("pump_buy")).await;
    h.tg.clear();
    // Four minutes after the entity's buy, it sells the same token.
    h.advance(240);
    h.see(WALLET_2, &as_wallet("jup_sell", WALLET_2, "sigSideSell", T0 + 240)).await;

    let flagged = h.store.entity(1).await.unwrap().unwrap();
    assert!(flagged.bait && !flagged.copyable);
    assert!(h.inbox(FRIEND).contains("flagged as <b>bait</b>") && h.inbox(FRIEND).contains("Auto-copy of this entity is stopped"), "{}", h.inbox(FRIEND));
    assert!(h.say(FRIEND, "/entity 1").await[0].contains("flagged as bait"));
    // The next buy is a card, not a copy.
    let before = positions(&h, FRIEND).await.len();
    h.tg.clear();
    h.advance(900);
    h.see(WALLET, &as_wallet("pump_buy", WALLET, "sigAfterBait", T0 + 1_200)).await;
    assert_eq!(positions(&h, FRIEND).await.len(), before);
    assert!(h.inbox(FRIEND).contains("Not copied: this entity is flagged as bait"), "{}", h.inbox(FRIEND));
    // Told once, not on every sell.
    h.tg.clear();
    h.see(WALLET_2, &as_wallet("jup_sell", WALLET_2, "sigSideSell2", T0 + 1_300)).await;
    assert!(!h.inbox(FRIEND).contains("bait"));
}
