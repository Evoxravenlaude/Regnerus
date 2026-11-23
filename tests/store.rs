//! One suite, two stores. The in-memory store always runs. The Postgres
//! store runs when TEST_DATABASE_URL points at a database, each run in a
//! schema of its own that is dropped afterwards.

mod common;

use std::sync::Arc;

use regnerus::model::*;
use regnerus::store::Store;
use regnerus::store::memory::MemoryStore;
use regnerus::store::pg::{PgStore, is_transaction_pooler, valid_schema};
use serde_json::json;

const A: &str = "FwySUYJVsjuaRpPTEgqmDXnSQgTNFEnQxcxF42oaMcb4";
const B: &str = "ULwVZmCfFXz4AhuiPmwJgBt873EuSHNE3VjpmVoPsaa";
const C: &str = "61pPQoBzka9SVZd664KCzAPHbEmwnhkb13V38mg1PUfy";
const MINT: &str = "2cHNNrQKqt4nvKFLB7iVUGA5UMngdgBJc1PGrtNh2c8W";

fn wallet(address: &str, entity: Option<EntityId>, state: WalletState, linked_by: Option<LinkedBy>, first_seen: i64) -> Wallet {
    Wallet { address: address.into(), entity_id: entity, state, score: 0, linked_by, first_seen, last_signature: None }
}

fn trade(signature: &str, address: &str, entity: Option<EntityId>, slot: u64) -> Trade {
    Trade {
        signature: signature.into(),
        slot,
        block_time: Some(1_759_400_000),
        seen_at: 1_759_400_002,
        wallet: address.into(),
        entity_id: entity,
        side: Side::Buy,
        mint: MINT.into(),
        token_amount_raw: u64::MAX - 7, // larger than a signed 64-bit column
        decimals: 6,
        token_program: Some(TokenProgram::SplToken),
        quote: Quote::Sol,
        quote_amount_raw: 101_000_000,
        programs: vec!["6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".into()],
        venue: Venue::PumpFun,
        fingerprint: Fingerprint {
            fee_payer: address.into(),
            self_paid: true,
            wallet_signed: true,
            cu_limit: Some(80_000),
            cu_price: Some(1_000_000),
            priority_fee_lamports: 10_000,
            tip_lamports: 1_000_000,
            tip_account: Some(C.into()),
            tip_family: Some("jito".into()),
            app: None,
            app_fee_lamports: 0,
        },
    }
}

async fn suite(store: Arc<dyn Store>) {
    // ---- accounts and invites ----
    assert_eq!(store.account(1).await.unwrap(), None);
    let admin = store.upsert_account(1, "sage", true, 100).await.unwrap();
    assert_eq!(admin, Account { id: 1, name: "sage".into(), is_admin: true, paused: false, created_at: 100 });
    let again = store.upsert_account(1, "sage2", true, 999).await.unwrap();
    assert_eq!((again.name.as_str(), again.created_at), ("sage2", 100), "the name follows, the creation date stays");
    store.upsert_account(2, "friend", false, 200).await.unwrap();
    store.set_paused(2, true).await.unwrap();
    assert!(store.account(2).await.unwrap().unwrap().paused);
    let again = store.upsert_account(2, "friend", false, 300).await.unwrap();
    assert!(again.paused, "an upsert does not clear a pause");
    assert_eq!(store.accounts().await.unwrap().iter().map(|a| a.id).collect::<Vec<_>>(), vec![1, 2]);

    store.create_invite("CODE1", 2, 1, 100).await.unwrap();
    assert!(store.redeem_invite("CODE1").await.unwrap());
    assert!(store.redeem_invite("CODE1").await.unwrap());
    assert!(!store.redeem_invite("CODE1").await.unwrap(), "used up");
    assert!(!store.redeem_invite("NOPE").await.unwrap());

    // ---- entities ----
    let e1 = store.create_entity("whale", 1, 100).await.unwrap();
    let e2 = store.create_entity("cabal", 2, 110).await.unwrap();
    assert!(e2.id > e1.id);
    assert_eq!(e1.status, EntityStatus::Tracking);
    assert!(!e1.copyable && !e1.bait);
    assert_eq!(store.entity(e1.id).await.unwrap().unwrap(), e1);
    assert_eq!(store.entity(999_999).await.unwrap(), None);
    assert!(store.rename_entity(e1.id, "big whale").await.unwrap());
    assert!(!store.rename_entity(999_999, "x").await.unwrap());
    assert_eq!(store.entities().await.unwrap().iter().map(|e| e.label.clone()).collect::<Vec<_>>(), vec!["big whale", "cabal"]);

    // ---- wallets ----
    store.put_wallet(&wallet(A, Some(e1.id), WalletState::Linked, Some(LinkedBy::Root), 100)).await.unwrap();
    store.put_wallet(&wallet(B, Some(e1.id), WalletState::Candidate, None, 120)).await.unwrap();
    store.put_wallet(&wallet(C, Some(e2.id), WalletState::Seen, None, 130)).await.unwrap();
    store.set_cursor(A, "sig-newest").await.unwrap();
    // Updating a wallet keeps when it was first seen and its cursor.
    let mut moved = wallet(A, Some(e1.id), WalletState::Linked, Some(LinkedBy::Admin), 5_000);
    moved.score = 70;
    store.put_wallet(&moved).await.unwrap();
    let a = store.wallet(A).await.unwrap().unwrap();
    assert_eq!((a.first_seen, a.last_signature.as_deref(), a.score, a.linked_by), (100, Some("sig-newest"), 70, Some(LinkedBy::Admin)));
    assert_eq!(store.wallet("missing").await.unwrap(), None);
    assert_eq!(store.wallets_of(e1.id).await.unwrap().iter().map(|w| w.address.as_str()).collect::<Vec<_>>(), vec![A, B]);

    let watched: Vec<String> = store.watched_wallets().await.unwrap().into_iter().map(|w| w.address).collect();
    assert_eq!(watched.len(), 2, "linked and candidate wallets are watched; seen ones are not");
    assert!(watched.contains(&A.to_string()) && watched.contains(&B.to_string()));
    assert!(store.set_entity_status(e1.id, EntityStatus::Retired).await.unwrap());
    assert!(store.watched_wallets().await.unwrap().is_empty(), "a retired entity's wallets stop being watched");
    assert!(store.set_entity_status(e1.id, EntityStatus::Tracking).await.unwrap());

    // ---- evidence ----
    let ev = NewEvidence {
        wallet: A.into(),
        entity_id: e1.id,
        kind: "root".into(),
        family: "manual".into(),
        points: 100,
        signatures: vec!["s1".into(), "s2".into()],
        details: json!({ "by": 1 }),
        source: "admin".into(),
        observed_at: 100,
    };
    let id1 = store.add_evidence(&ev).await.unwrap();
    let id2 = store.add_evidence(&NewEvidence { kind: "cobuy".into(), family: "cobuy".into(), points: 12, ..ev.clone() }).await.unwrap();
    assert!(id2 > id1);
    let rows = store.evidence_of_wallet(A).await.unwrap();
    assert_eq!(rows.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), vec!["cobuy", "root"], "newest first");
    assert_eq!(rows[1].signatures, vec!["s1", "s2"]);
    assert_eq!(rows[1].details, json!({ "by": 1 }));
    assert_eq!(store.void_evidence(A, e1.id).await.unwrap(), 2);
    assert_eq!(store.void_evidence(A, e1.id).await.unwrap(), 0, "already voided");
    assert!(store.evidence_of_wallet(A).await.unwrap().iter().all(|e| e.voided), "voided rows are kept");

    // ---- follows ----
    store.set_follow(1, e1.id, FollowMode::Cards).await.unwrap();
    store.set_follow(2, e1.id, FollowMode::Cards).await.unwrap();
    store.set_follow(2, e1.id, FollowMode::Off).await.unwrap();
    store.set_follow(2, e2.id, FollowMode::Auto).await.unwrap();
    let of_entity = store.follows_of_entity(e1.id).await.unwrap();
    assert_eq!(of_entity.iter().map(|f| (f.account, f.mode)).collect::<Vec<_>>(), vec![(1, FollowMode::Cards), (2, FollowMode::Off)]);
    assert_eq!(store.follows_of_account(2).await.unwrap().len(), 2);

    // ---- labels ----
    let label = Label { address: C.into(), class: LabelClass::CexHot, name: "binance".into(), source: "admin".into() };
    store.put_label(&label).await.unwrap();
    store.put_label(&Label { name: "binance 2".into(), ..label.clone() }).await.unwrap();
    assert_eq!(store.labels().await.unwrap(), vec![Label { name: "binance 2".into(), ..label.clone() }]);
    assert!(store.delete_label(C).await.unwrap());
    assert!(!store.delete_label(C).await.unwrap());

    // ---- trades ----
    let t1 = trade("sig1", A, Some(e1.id), 10);
    assert!(store.insert_trade(&t1).await.unwrap());
    assert!(!store.insert_trade(&t1).await.unwrap(), "the same signature, wallet and mint is stored once");
    assert!(store.insert_trade(&trade("sig2", A, Some(e1.id), 12)).await.unwrap());
    assert!(store.insert_trade(&trade("sig3", B, Some(e1.id), 11)).await.unwrap());
    let of_wallet = store.trades_of_wallet(A, 10).await.unwrap();
    assert_eq!(of_wallet.iter().map(|t| t.signature.as_str()).collect::<Vec<_>>(), vec!["sig2", "sig1"], "newest first");
    assert_eq!(of_wallet[1], t1, "a trade reads back exactly as written, fingerprint and large amount included");
    assert_eq!(store.trades_of_wallet(A, 1).await.unwrap().len(), 1);
    // B is only a candidate: its trades are not yet the entity's.
    assert_eq!(store.trades_of_entity(e1.id, 10).await.unwrap().iter().map(|t| t.signature.as_str()).collect::<Vec<_>>(), vec!["sig2", "sig1"]);
    store.put_wallet(&wallet(B, Some(e1.id), WalletState::Linked, Some(LinkedBy::Admin), 120)).await.unwrap();
    assert_eq!(store.trades_of_entity(e1.id, 10).await.unwrap().iter().map(|t| t.signature.as_str()).collect::<Vec<_>>(), vec!["sig2", "sig3", "sig1"], "linking a wallet brings its history with it");

    // ---- transfers ----
    let sol = Transfer { signature: "x1".into(), slot: 20, block_time: None, wallet: A.into(), direction: Direction::In, counterparty: C.into(), mint: None, amount_raw: 500_000_000, decimals: 9 };
    assert!(store.insert_transfer(&sol).await.unwrap());
    assert!(!store.insert_transfer(&sol).await.unwrap());
    let token = Transfer { mint: Some(MINT.into()), direction: Direction::Out, amount_raw: u64::MAX, decimals: 6, slot: 21, ..sol.clone() };
    assert!(store.insert_transfer(&token).await.unwrap(), "same signature, a different mint and direction");
    assert_eq!(store.transfers_of_wallet(A, 10).await.unwrap(), vec![token, sol]);

    // ---- signals ----
    let fresh = Signal {
        id: 0,
        entity_id: e1.id,
        mint: MINT.into(),
        side: Side::Buy,
        first_at: 1_000,
        last_at: 1_000,
        quote: Quote::Sol,
        quote_amount_raw: 101_000_000,
        token_amount_raw: 3_500_000_000_000,
        decimals: 6,
        wallets: vec![A.into()],
        trades: 1,
        venue: Venue::PumpFun,
    };
    assert_eq!(store.open_signal(e1.id, MINT, Side::Buy, 0).await.unwrap(), None);
    let mut s = store.create_signal(&fresh).await.unwrap();
    assert!(s.id > 0);
    assert_eq!(store.open_signal(e1.id, MINT, Side::Buy, 970).await.unwrap(), Some(s.clone()));
    assert_eq!(store.open_signal(e1.id, MINT, Side::Buy, 1_001).await.unwrap(), None, "outside the window");
    assert_eq!(store.open_signal(e1.id, MINT, Side::Sell, 0).await.unwrap(), None, "a sell is its own signal");
    assert_eq!(store.open_signal(e2.id, MINT, Side::Buy, 0).await.unwrap(), None);
    s.last_at = 1_020;
    s.quote_amount_raw += 50_000_000;
    s.wallets.push(B.into());
    s.trades = 2;
    store.update_signal(&s).await.unwrap();
    assert_eq!(store.open_signal(e1.id, MINT, Side::Buy, 1_010).await.unwrap(), Some(s.clone()));
    store.set_signal_message(s.id, 1, 555).await.unwrap();
    store.set_signal_message(s.id, 2, 556).await.unwrap();
    store.set_signal_message(s.id, 2, 557).await.unwrap();
    assert_eq!(store.signal_messages(s.id).await.unwrap(), vec![(1, 555), (2, 557)]);

    // ---- state, credits, counts ----
    assert_eq!(store.get_state("k").await.unwrap(), None);
    store.set_state("k", "v1").await.unwrap();
    store.set_state("k", "v2").await.unwrap();
    assert_eq!(store.get_state("k").await.unwrap().as_deref(), Some("v2"));
    store.add_credits("2026-11-22", "watch", 10).await.unwrap();
    store.add_credits("2026-11-22", "watch", 5).await.unwrap();
    store.add_credits("2026-11-22", "copy", 1).await.unwrap();
    store.add_credits("2026-11-21", "watch", 99).await.unwrap();
    assert_eq!(store.credits("2026-11-22").await.unwrap(), vec![("copy".to_string(), 1), ("watch".to_string(), 15)]);
    let counts = store.counts().await.unwrap();
    assert_eq!((counts.entities, counts.wallets, counts.trades, counts.transfers), (2, 3, 3, 2));
}

/// What phases 2 to 6 added: evidence edits, deposit addresses, the job
/// queue, documents, positions and leases. Runs after `suite`, on its own rows.
async fn later_phases(store: Arc<dyn Store>) {
    const W1: &str = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";
    const W2: &str = "H8sMJSCQxfKiFTCfDR3DUMLPwcRbM61LGFJ8N4dK3WjS";
    const DEPOSIT: &str = "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9";
    const MINT_2: &str = "H5kxvHgqsQabgjvuLPqBHWnLWteiLCuDY9ZyXpL1oe8z";
    let e = store.create_entity("later", 1, 1_000).await.unwrap().id;
    let f = store.create_entity("other", 1, 1_000).await.unwrap().id;
    let row = |wallet: &str, entity: EntityId, kind: &str, points: i32| NewEvidence {
        wallet: wallet.into(),
        entity_id: entity,
        kind: kind.into(),
        family: "funding".into(),
        points,
        signatures: vec![],
        details: json!({ "key": "k" }),
        source: "script".into(),
        observed_at: 5,
    };

    // ---- evidence: void one row, re-point a kind, move between entities ----
    let cobuy = store.add_evidence(&row(W1, e, "cobuy_slots", 12)).await.unwrap();
    let funding = store.add_evidence(&row(W1, e, "direct_funding", 45)).await.unwrap();
    store.add_evidence(&row(W2, f, "direct_funding", 45)).await.unwrap();
    assert!(store.void_evidence_row(cobuy).await.unwrap());
    assert!(!store.void_evidence_row(cobuy).await.unwrap(), "already voided");
    assert_eq!(store.repoint_evidence("direct_funding", 50).await.unwrap(), 2);
    assert_eq!(store.repoint_evidence("direct_funding", 50).await.unwrap(), 0, "nothing left to change");
    assert_eq!(store.repoint_evidence("cobuy_slots", 9).await.unwrap(), 0, "voided rows keep their points");
    let of_e = store.evidence_of_entity(e).await.unwrap();
    assert_eq!(of_e.iter().map(|r| (r.id, r.points, r.voided)).collect::<Vec<_>>(), vec![(funding, 50, false), (cobuy, 12, true)], "newest first");
    assert_eq!(store.move_evidence(e, f).await.unwrap(), 2);
    assert_eq!((store.evidence_of_entity(e).await.unwrap().len(), store.evidence_of_entity(f).await.unwrap().len()), (0, 3));

    // ---- wallets by state, entity flags ----
    store.put_wallet(&wallet(W1, Some(e), WalletState::Candidate, None, 10)).await.unwrap();
    assert!(store.wallets_in_state(WalletState::Candidate).await.unwrap().iter().any(|w| w.address == W1));
    assert!(!store.wallets_in_state(WalletState::Rejected).await.unwrap().iter().any(|w| w.address == W1));
    store.set_entity_flags(e, Some(true), None).await.unwrap();
    store.set_entity_flags(e, None, Some(true)).await.unwrap();
    let flagged = store.entity(e).await.unwrap().unwrap();
    assert!(flagged.copyable && flagged.bait, "a flag left out stays as it was");

    // ---- trades of a token, signals since a time ----
    let mut early = trade("sigMintEarly", W1, Some(e), 10);
    early.mint = MINT_2.into();
    early.block_time = Some(5_000);
    let mut late = trade("sigMintLate", W2, Some(f), 20);
    late.mint = MINT_2.into();
    late.block_time = Some(6_000);
    store.insert_trade(&late).await.unwrap();
    store.insert_trade(&early).await.unwrap();
    let of_mint = store.trades_of_mint(MINT_2, 0, 10).await.unwrap();
    assert_eq!(of_mint.iter().map(|t| t.signature.as_str()).collect::<Vec<_>>(), vec!["sigMintEarly", "sigMintLate"], "oldest first");
    assert_eq!(store.trades_of_mint(MINT_2, 5_500, 10).await.unwrap().len(), 1);
    assert_eq!(store.trades_of_mint(MINT_2, 0, 1).await.unwrap().len(), 1);
    let made = store
        .create_signal(&Signal {
            id: 0,
            entity_id: e,
            mint: MINT_2.into(),
            side: Side::Buy,
            first_at: 9_000_000_000,
            last_at: 9_000_000_005,
            quote: Quote::Sol,
            quote_amount_raw: 1,
            token_amount_raw: 2,
            decimals: 6,
            wallets: vec![W1.into()],
            trades: 1,
            venue: Venue::PumpFun,
        })
        .await
        .unwrap();
    assert_eq!(store.signal(made.id).await.unwrap(), Some(made.clone()));
    assert_eq!(store.signal(made.id + 1_000).await.unwrap(), None);
    assert_eq!(store.signals_since(9_000_000_005).await.unwrap(), vec![made.clone()]);
    assert!(store.signals_since(9_000_000_006).await.unwrap().is_empty());

    // ---- deposit addresses ----
    let deposit = DepositAddr { address: DEPOSIT.into(), entity_id: e, exchange: "Binance".into(), status: DepositStatus::Pending, senders: 2, first_seen: 100 };
    store.put_deposit(&deposit).await.unwrap();
    assert_eq!(store.deposit(DEPOSIT).await.unwrap(), Some(deposit.clone()));
    assert!(store.watched_deposits().await.unwrap().is_empty(), "only qualified addresses are tripwires");
    store.put_deposit(&DepositAddr { status: DepositStatus::Qualified, senders: 3, first_seen: 999, ..deposit.clone() }).await.unwrap();
    let stored = store.deposit(DEPOSIT).await.unwrap().unwrap();
    assert_eq!((stored.status, stored.senders, stored.first_seen), (DepositStatus::Qualified, 3, 100), "first seen is kept");
    assert_eq!(store.deposits_of(e).await.unwrap().len(), 1);
    assert_eq!(store.watched_deposits().await.unwrap().iter().filter(|d| d.address == DEPOSIT).count(), 1);
    store.set_entity_status(e, EntityStatus::Retired).await.unwrap();
    assert!(store.watched_deposits().await.unwrap().iter().all(|d| d.address != DEPOSIT), "a retired entity's tripwires are dropped");
    store.set_entity_status(e, EntityStatus::Tracking).await.unwrap();

    // ---- the job queue ----
    let job = NewJob { kind: "trace_back".into(), subject: W1.into(), entity_id: Some(e), payload: json!({ "hops": 2 }), run_after: 500 };
    let first = store.enqueue_job(&job, 400).await.unwrap().expect("queued");
    assert_eq!(store.enqueue_job(&job, 401).await.unwrap(), None, "one live job per kind and subject");
    store.enqueue_job(&NewJob { kind: "inspect".into(), subject: W1.into(), run_after: 450, ..job.clone() }, 402).await.unwrap().expect("another kind is another job");
    assert_eq!(store.claim_job(449).await.unwrap(), None, "nothing is due yet");
    let claimed = store.claim_job(600).await.unwrap().unwrap();
    assert_eq!((claimed.kind.as_str(), claimed.status, claimed.attempts), ("inspect", JobStatus::Running, 1), "the earliest due job first");
    let claimed = store.claim_job(600).await.unwrap().unwrap();
    assert_eq!((claimed.id, claimed.payload["hops"].as_i64(), claimed.entity_id, claimed.created_at), (first, Some(2), Some(e), 400));
    assert_eq!(store.claim_job(600).await.unwrap(), None);
    assert_eq!(store.job_counts().await.unwrap(), (0, 2));
    store.retry_job(first, 700, &json!({ "hops": 1 })).await.unwrap();
    assert_eq!(store.job_counts().await.unwrap(), (1, 1));
    assert_eq!(store.claim_job(650).await.unwrap(), None, "a retried job waits for its time");
    let again = store.claim_job(700).await.unwrap().unwrap();
    assert_eq!((again.id, again.attempts, again.payload["hops"].as_i64()), (first, 2, Some(1)));
    store.finish_job(first, true).await.unwrap();
    assert!(store.enqueue_job(&job, 800).await.unwrap().is_some(), "a finished job no longer blocks a new one");

    // ---- documents ----
    assert_eq!(store.doc("funder", W1).await.unwrap(), None);
    store.put_doc("funder", W1, &json!({ "funder": W2, "lamports": 5 }), 10).await.unwrap();
    store.put_doc("funder", W1, &json!({ "funder": W2, "lamports": 6 }), 11).await.unwrap();
    store.put_doc("funder", W2, &json!({ "funder": null }), 12).await.unwrap();
    store.put_doc("cobuy", &format!("{W1}:1:x"), &json!(1), 13).await.unwrap();
    assert_eq!(store.doc("funder", W1).await.unwrap().unwrap()["lamports"], json!(6), "a second put replaces");
    let listed = store.docs("funder", "", 10).await.unwrap();
    assert_eq!(listed.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), vec![W1, W2], "by key, one kind only");
    assert_eq!(store.docs("funder", "H8", 10).await.unwrap().len(), 1, "a key prefix narrows it");
    assert_eq!(store.docs("funder", "", 1).await.unwrap().len(), 1);
    assert_eq!(store.docs("funder", "%", 10).await.unwrap().len(), 0, "a prefix is text, not a pattern");
    assert!(store.delete_doc("funder", W2).await.unwrap());
    assert!(!store.delete_doc("funder", W2).await.unwrap());

    // ---- positions ----
    let position = Position {
        id: 0,
        account: 1,
        entity_id: e,
        signal_id: made.id,
        mint: MINT_2.into(),
        decimals: 6,
        mode: PositionMode::Paper,
        status: PositionStatus::Opening,
        entry_lamports: 50_000_000,
        tokens_raw: 0,
        tokens_left: 0,
        exit_lamports: 0,
        opened_at: 9_000_000_010,
        closed_at: None,
        exit_reason: None,
        open_signature: None,
        note: String::new(),
    };
    let id = store.insert_position(&position).await.unwrap().expect("stored");
    assert_eq!(store.insert_position(&position).await.unwrap(), None, "one copy of a signal per account and mode");
    let shadow = store.insert_position(&Position { account: SHADOW_ACCOUNT, mode: PositionMode::Shadow, ..position.clone() }).await.unwrap().expect("the shadow copy is its own row");
    assert!(store.insert_position(&Position { mode: PositionMode::Live, ..position.clone() }).await.unwrap().is_some(), "live and paper are separate");
    assert_eq!(store.open_positions().await.unwrap().len(), 3);
    let filled = Position {
        id,
        status: PositionStatus::Open,
        tokens_raw: u64::MAX - 1,
        tokens_left: u64::MAX - 1,
        note: "price impact 0.10%".into(),
        open_signature: Some("sigOpen".into()),
        ..position.clone()
    };
    store.update_position(&filled).await.unwrap();
    assert_eq!(store.position(id).await.unwrap(), Some(filled.clone()), "token amounts above a signed 64-bit survive");
    let closed = Position { status: PositionStatus::Closed, tokens_left: 0, exit_lamports: 80_000_000, closed_at: Some(9_000_000_100), exit_reason: Some("take_profit".into()), ..filled.clone() };
    store.update_position(&closed).await.unwrap();
    assert_eq!(store.position(id).await.unwrap().unwrap().pnl_lamports(), 30_000_000);
    assert_eq!(store.open_positions().await.unwrap().len(), 2, "a closed position is no longer open");
    assert_eq!(store.positions_of_account(1, 0).await.unwrap().len(), 2);
    assert!(store.positions_of_account(1, 9_000_000_011).await.unwrap().is_empty());
    assert_eq!(store.positions_of_entity(e, PositionMode::Shadow).await.unwrap().iter().map(|p| p.id).collect::<Vec<_>>(), vec![shadow]);

    // ---- leases ----
    assert!(store.acquire_lease("executor", "a", 60, 100).await.unwrap());
    assert!(!store.acquire_lease("executor", "b", 60, 120).await.unwrap(), "held by someone else");
    assert!(store.acquire_lease("executor", "a", 60, 130).await.unwrap(), "the holder renews");
    assert!(!store.acquire_lease("executor", "b", 60, 190).await.unwrap(), "renewed until 190");
    assert!(store.acquire_lease("executor", "b", 60, 191).await.unwrap(), "expired: free to take");
    assert!(store.acquire_lease("other", "a", 60, 191).await.unwrap(), "another lease is another matter");
}

#[tokio::test]
async fn memory_store_passes_the_suite() {
    let store = Arc::new(MemoryStore::new());
    suite(store.clone()).await;
    later_phases(store).await;
}

#[tokio::test]
async fn postgres_store_passes_the_suite() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL is not set; skipping the Postgres run");
        return;
    };
    let schema = format!("t_{}_{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos());
    let store = Arc::new(PgStore::connect(&url, &schema).await.expect("connect"));
    assert_eq!(store.migrate().await.expect("migrate"), vec![1, 2]);
    assert_eq!(store.migrate().await.expect("migrate twice"), Vec::<i32>::new(), "a second run applies nothing");
    suite(store.clone()).await;
    later_phases(store.clone()).await;
    store.drop_schema(&schema).await.expect("drop");
    store.close().await;
}

#[test]
fn schema_names_are_held_to_a_strict_shape() {
    assert!(valid_schema("regnerus"));
    assert!(valid_schema("t_1_2"));
    for bad in ["", "Public", "a-b", "a b", "1abc", "x\"; drop schema public; --", "drop;"] {
        assert!(!valid_schema(bad), "{bad:?}");
    }
}

#[tokio::test]
async fn a_transaction_pooler_url_is_refused_before_connecting() {
    assert!(is_transaction_pooler("postgres://u:p@aws-0.pooler.supabase.com:6543/postgres"));
    assert!(is_transaction_pooler("postgres://u:p@host:5432/db?pgbouncer=true"));
    assert!(!is_transaction_pooler("postgres://u:p@aws-0.pooler.supabase.com:5432/postgres"));
    let error = match PgStore::connect("postgres://u:p@127.0.0.1:6543/postgres", "regnerus").await {
        Ok(_) => panic!("connected to a transaction pooler"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("port 5432"), "{error}");
    assert!(!error.contains("u:p"), "the URL is not echoed: {error}");
}
