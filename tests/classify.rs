//! The classifier is pure: a transaction in, a trade, transfers or nothing
//! out. The first tests are Yeks's own, run against the Rust port.

mod common;

use common::*;
use regnerus::chain::solana::labels::{JITO_TIP_ACCOUNTS, LabelBook};
use regnerus::chain::solana::tx::*;
use regnerus::model::{Direction, Label, LabelClass, Quote, Side, TokenProgram, Venue};
use regnerus::util::is_solana_address;
use serde_json::json;

#[test]
fn fixtures_and_program_tables_are_well_formed() {
    for (id, _) in VENUE_PROGRAMS {
        assert!(is_solana_address(id), "{id}");
    }
    for id in [USDC_MINT, USDT_MINT, WSOL_MINT, TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, COMPUTE_BUDGET_PROGRAM, SYSTEM_PROGRAM] {
        assert!(is_solana_address(id), "{id}");
    }
    for id in JITO_TIP_ACCOUNTS {
        assert!(is_solana_address(id), "{id}");
    }
    for name in FIXTURE_NAMES {
        let tx = parsed(&fixture(name));
        let meta = tx.meta.as_ref().unwrap();
        let keys = &tx.transaction.message.account_keys;
        for key in keys {
            assert!(is_solana_address(key.pubkey()), "{name}: {}", key.pubkey());
        }
        assert_eq!(meta.pre_balances.len(), keys.len(), "{name}");
        assert_eq!(meta.post_balances.len(), keys.len(), "{name}");
        let pre: u64 = meta.pre_balances.iter().sum();
        let post: u64 = meta.post_balances.iter().sum();
        assert_eq!(pre - post, meta.fee, "{name}: lamports are conserved except the fee");
    }
}

#[test]
fn pump_buy_is_a_buy_in_sol_net_of_fee_and_rent() {
    let tx = parsed(&fixture("pump_buy"));
    let t = classify_trade(&tx, WALLET).expect("a trade");
    assert_eq!(t.side, Side::Buy);
    assert_eq!(t.mint, MEME_MINT);
    assert_eq!(t.token_amount_raw, 3_500_000_000_000);
    assert_eq!(t.decimals, 6);
    assert_eq!(t.quote, Quote::Sol);
    assert_eq!(t.quote_amount_raw, 101_000_000, "0.1 SOL + 1% fee, not the rent or the tx fee");
    assert_eq!(t.token_program, Some(TokenProgram::SplToken));
    assert_eq!(t.venue, Venue::PumpFun);
    assert_eq!(t.programs, vec![PUMP.to_string()], "plumbing programs are dropped");
    assert_eq!(t.signature, SIG_PUMP_BUY);
    assert_eq!(t.slot, 300_100_000);
    assert_eq!(t.block_time, Some(1_759_400_000));
}

#[test]
fn jupiter_sell_to_usdc_is_a_sell_quoted_in_dollars() {
    let tx = parsed(&fixture("jup_sell"));
    let t = classify_trade(&tx, WALLET).expect("a trade");
    assert_eq!(t.side, Side::Sell);
    assert_eq!(t.mint, MEME_MINT);
    assert_eq!(t.token_amount_raw, 1_000_000_000_000_000);
    assert_eq!(t.decimals, 9);
    assert_eq!(t.quote, Quote::Usdc);
    assert_eq!(t.quote_amount_raw, 250_500_000);
    assert_eq!(t.venue, Venue::Jupiter);
    assert_eq!(t.programs, vec![JUPITER.to_string(), RAYDIUM_AMM.to_string()]);
}

#[test]
fn a_plain_sol_transfer_is_not_a_trade_for_either_side() {
    let tx = parsed(&fixture("sol_transfer"));
    assert_eq!(classify_trade(&tx, WALLET), None);
    assert_eq!(classify_trade(&tx, OTHER), None);
}

#[test]
fn a_token_transfer_out_is_not_a_trade_and_neither_is_the_airdrop_side() {
    let tx = parsed(&fixture("token_out"));
    assert_eq!(classify_trade(&tx, WALLET), None);
    assert_eq!(classify_trade(&tx, OTHER), None, "the recipient got tokens for nothing");
}

#[test]
fn token_2022_buy_from_wrapped_sol_counts_wsol_as_sol() {
    let tx = parsed(&fixture("t22_buy"));
    let t = classify_trade(&tx, WALLET).expect("a trade");
    assert_eq!(t.side, Side::Buy);
    assert_eq!(t.mint, T22_MINT);
    assert_eq!(t.token_amount_raw, 12_000_000_000_000);
    assert_eq!(t.decimals, 9);
    assert_eq!(t.quote, Quote::Sol);
    assert_eq!(t.quote_amount_raw, 250_000_000, "the new account's rent is not part of the price");
    assert_eq!(t.token_program, Some(TokenProgram::Token2022));
    assert_eq!(t.venue, Venue::Meteora);
    assert_eq!(t.programs, vec![METEORA_DBC.to_string()]);
}

#[test]
fn failed_transactions_unmentioned_wallets_and_missing_meta_are_none() {
    let mut failed = fixture("pump_buy");
    failed["meta"]["err"] = json!({ "InstructionError": [3, { "Custom": 6002 }] });
    assert_eq!(classify_trade(&parsed(&failed), WALLET), None);
    assert_eq!(classify(&parsed(&failed), WALLET), Classified::Other, "a failed transaction moved nothing");

    assert_eq!(classify_trade(&parsed(&fixture("pump_buy")), WALLET_2), None, "not in accountKeys");

    let mut no_meta = fixture("pump_buy");
    no_meta["meta"] = json!(null);
    assert_eq!(classify_trade(&parsed(&no_meta), WALLET), None);
}

#[test]
fn stable_swaps_multi_mint_routes_and_dust_are_none() {
    // USDC -> USDT: both are quotes, nothing was bought.
    let mut stable = fixture("jup_sell");
    stable["meta"]["preTokenBalances"][0]["mint"] = json!(USDC_MINT);
    stable["meta"]["preTokenBalances"][0]["uiTokenAmount"] = json!({ "amount": "300000000", "decimals": 6 });
    stable["meta"]["postTokenBalances"][0]["mint"] = json!(USDC_MINT);
    stable["meta"]["postTokenBalances"][0]["uiTokenAmount"] = json!({ "amount": "49500000", "decimals": 6 });
    stable["meta"]["preTokenBalances"][1]["mint"] = json!(USDT_MINT);
    stable["meta"]["postTokenBalances"][1]["mint"] = json!(USDT_MINT);
    assert_eq!(classify_trade(&parsed(&stable), WALLET), None);

    // Two non-quote mints moving for the wallet: ambiguous, skipped.
    let mut multi = fixture("jup_sell");
    multi["meta"]["preTokenBalances"][1]["mint"] = json!(MEME_MINT_2);
    multi["meta"]["postTokenBalances"][1]["mint"] = json!(MEME_MINT_2);
    assert_eq!(classify_trade(&parsed(&multi), WALLET), None);

    // A buy whose SOL leg is under 0.005 SOL once rent is excluded.
    let mut dust = fixture("pump_buy");
    dust["meta"]["postBalances"][0] = json!(1_000_000_000u64 - 15_000 - 2_039_280 - 1_000_000);
    assert_eq!(classify_trade(&parsed(&dust), WALLET), None);
}

#[test]
fn account_keys_as_plain_strings_are_read_too() {
    let mut plain = fixture("pump_buy");
    let keys: Vec<serde_json::Value> = plain["transaction"]["message"]["accountKeys"].as_array().unwrap().iter().map(|k| k["pubkey"].clone()).collect();
    plain["transaction"]["message"]["accountKeys"] = json!(keys);
    let t = classify_trade(&parsed(&plain), WALLET).expect("a trade");
    assert_eq!(t.quote_amount_raw, 101_000_000);
}

// ---- transfers ----

#[test]
fn a_sol_transfer_is_out_for_the_sender_and_in_for_the_receiver() {
    let tx = parsed(&fixture("sol_transfer"));
    let out = extract_transfers(&tx, WALLET);
    assert_eq!(out, vec![TransferCore { direction: Direction::Out, counterparty: OTHER.into(), mint: None, amount_raw: 500_000_000, decimals: 9 }]);
    let inn = extract_transfers(&tx, OTHER);
    assert_eq!(inn, vec![TransferCore { direction: Direction::In, counterparty: WALLET.into(), mint: None, amount_raw: 500_000_000, decimals: 9 }]);
    assert!(matches!(classify(&tx, WALLET), Classified::Transfers(_)));
}

#[test]
fn a_token_transfer_names_the_other_owner_not_the_token_account() {
    let tx = parsed(&fixture("token_out"));
    let out = extract_transfers(&tx, WALLET);
    assert_eq!(
        out,
        vec![TransferCore { direction: Direction::Out, counterparty: OTHER.into(), mint: Some(MEME_MINT.into()), amount_raw: 1_000_000_000, decimals: 6 }],
        "the rent paid for the recipient's account is not a transfer to anyone"
    );
    let inn = extract_transfers(&tx, OTHER);
    assert_eq!(inn, vec![TransferCore { direction: Direction::In, counterparty: WALLET.into(), mint: Some(MEME_MINT.into()), amount_raw: 1_000_000_000, decimals: 6 }]);
}

#[test]
fn sol_dust_is_dropped_and_a_trade_is_never_reported_as_transfers() {
    let mut dust = fixture("sol_transfer");
    dust["transaction"]["message"]["instructions"][0]["parsed"]["info"]["lamports"] = json!(1);
    assert!(extract_transfers(&parsed(&dust), OTHER).is_empty(), "address-poisoning dust");
    assert_eq!(classify(&parsed(&dust), OTHER), Classified::Other);

    assert!(matches!(classify(&parsed(&fixture("pump_buy")), WALLET), Classified::Trade(_)));
    assert_eq!(classify(&parsed(&fixture("pump_buy")), WALLET_2), Classified::Other, "a wallet that is not in the transaction");
}

#[test]
fn lamports_sent_as_a_string_are_read() {
    let mut text = fixture("sol_transfer");
    text["transaction"]["message"]["instructions"][0]["parsed"]["info"]["lamports"] = json!("500000000");
    assert_eq!(extract_transfers(&parsed(&text), WALLET)[0].amount_raw, 500_000_000);
}

// ---- fingerprint ----

fn book() -> LabelBook {
    LabelBook::seeded()
}

#[test]
fn fingerprint_reads_compute_budget_fee_payer_and_priority_fee() {
    let tx = parsed(&fixture("pump_buy"));
    let labels = book();
    let print = fingerprint(&tx, WALLET, &|a| labels.get(a));
    assert_eq!(print.fee_payer, WALLET);
    assert!(print.self_paid);
    assert!(print.wallet_signed);
    assert_eq!(print.priority_fee_lamports, 10_000, "fee 15,000 less the 5,000 base fee");
    assert_eq!(print.tip_lamports, 0);
    assert_eq!(print.app, None);
    // The two compute-budget instructions in the fixture decode to a price and a limit.
    let decode = |data: &str| bs58::decode(data).into_vec().unwrap();
    let price_bytes = decode("3DTZbgwsozUF");
    let limit_bytes = decode("Fj2Eoy");
    assert_eq!(price_bytes[0], 3, "SetComputeUnitPrice");
    assert_eq!(limit_bytes[0], 2, "SetComputeUnitLimit");
    let mut price = [0u8; 8];
    price.copy_from_slice(&price_bytes[1..9]);
    assert_eq!(print.cu_price, Some(u64::from_le_bytes(price)));
    assert_eq!(print.cu_limit, Some(u32::from_le_bytes([limit_bytes[1], limit_bytes[2], limit_bytes[3], limit_bytes[4]])));
}

/// The pump.fun buy with a Jito tip and an app fee added, balances kept honest.
fn buy_with_tip_and_app_fee(app_fee_wallet: &str) -> serde_json::Value {
    let mut tx = fixture("pump_buy");
    let tip = 1_000_000u64;
    let app_fee = 1_010_000u64;
    let keys = tx["transaction"]["message"]["accountKeys"].as_array_mut().unwrap();
    keys.push(json!({ "pubkey": JITO_TIP, "signer": false, "writable": true, "source": "transaction" }));
    keys.push(json!({ "pubkey": app_fee_wallet, "signer": false, "writable": true, "source": "transaction" }));
    let instructions = tx["transaction"]["message"]["instructions"].as_array_mut().unwrap();
    for (to, lamports) in [(JITO_TIP, tip), (app_fee_wallet, app_fee)] {
        instructions.push(json!({ "program": "system", "programId": SYSTEM, "parsed": { "type": "transfer", "info": { "source": WALLET, "destination": to, "lamports": lamports } } }));
    }
    let pre0 = tx["meta"]["preBalances"][0].as_u64().unwrap();
    let post0 = tx["meta"]["postBalances"][0].as_u64().unwrap();
    assert!(pre0 > post0);
    tx["meta"]["postBalances"][0] = json!(post0 - tip - app_fee);
    for (amount, _) in [(tip, ()), (app_fee, ())] {
        tx["meta"]["preBalances"].as_array_mut().unwrap().push(json!(50_000_000u64));
        tx["meta"]["postBalances"].as_array_mut().unwrap().push(json!(50_000_000u64 + amount));
    }
    tx
}

#[test]
fn fingerprint_names_the_tip_family_and_the_app_from_labelled_wallets() {
    let app_wallet = OTHER;
    let tx = parsed(&buy_with_tip_and_app_fee(app_wallet));
    let meta = tx.meta.as_ref().unwrap();
    assert_eq!(meta.pre_balances.iter().sum::<u64>() - meta.post_balances.iter().sum::<u64>(), meta.fee, "the edited fixture still conserves lamports");

    let labels = book();
    labels.set(Label { address: app_wallet.into(), class: LabelClass::AppFee, name: "axiom".into(), source: "admin".into() });
    let print = fingerprint(&tx, WALLET, &|a| labels.get(a));
    assert_eq!(print.tip_lamports, 1_000_000);
    assert_eq!(print.tip_account.as_deref(), Some(JITO_TIP));
    assert_eq!(print.tip_family.as_deref(), Some("jito"));
    assert_eq!(print.app.as_deref(), Some("axiom"));
    assert_eq!(print.app_fee_lamports, 1_010_000);

    // The price the wallet paid includes what it tipped and paid the app:
    // that is what the trade cost it.
    let t = classify_trade(&tx, WALLET).expect("still a trade");
    assert_eq!(t.quote_amount_raw, 101_000_000 + 1_000_000 + 1_010_000);

    // With no label for the app wallet the app is unknown, not guessed.
    let bare = book();
    assert_eq!(fingerprint(&tx, WALLET, &|a| bare.get(a)).app, None);
}

#[test]
fn an_app_signed_trade_is_not_self_paid_and_takes_the_app_from_the_signer() {
    // The app's wallet becomes account 0 and pays the fee; the user's wallet moves to the end.
    let mut tx = fixture("pump_buy");
    let app_signer = WALLET_2;
    let keys = tx["transaction"]["message"]["accountKeys"].as_array_mut().unwrap();
    let mut user = keys.remove(0);
    user["signer"] = json!(false);
    keys.insert(0, json!({ "pubkey": app_signer, "signer": true, "writable": true, "source": "transaction" }));
    keys.push(user);
    let fee = tx["meta"]["fee"].as_u64().unwrap();
    for side in ["preBalances", "postBalances"] {
        let balances = tx["meta"][side].as_array_mut().unwrap();
        let user_balance = balances.remove(0).as_u64().unwrap();
        // The user no longer pays the fee: give it back on the post side.
        let user_balance = if side == "postBalances" { user_balance + fee } else { user_balance };
        balances.insert(0, json!(if side == "preBalances" { 10_000_000u64 } else { 10_000_000u64 - fee }));
        balances.push(json!(user_balance));
    }
    // Token balances point at account indexes, which all moved by none: index 0 was swapped, the user appended.
    let tx = parsed(&tx);
    let meta = tx.meta.as_ref().unwrap();
    assert_eq!(meta.pre_balances.iter().sum::<u64>() - meta.post_balances.iter().sum::<u64>(), meta.fee);

    let labels = book();
    labels.set(Label { address: app_signer.into(), class: LabelClass::AppSigner, name: "fomo".into(), source: "admin".into() });
    let print = fingerprint(&tx, WALLET, &|a| labels.get(a));
    assert_eq!(print.fee_payer, app_signer);
    assert!(!print.self_paid);
    assert!(!print.wallet_signed);
    assert_eq!(print.app.as_deref(), Some("fomo"));

    let t = classify_trade(&tx, WALLET).expect("the user's wallet still traded");
    assert_eq!(t.side, Side::Buy);
    assert_eq!(t.quote_amount_raw, 101_000_000, "no fee is added back for a wallet that did not pay it");
}
