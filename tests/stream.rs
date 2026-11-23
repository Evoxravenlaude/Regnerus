//! Subscription bookkeeping for the wallet stream, with no socket.

use regnerus::chain::solana::stream::SubBook;
use serde_json::{Value, json};

const A: &str = "FwySUYJVsjuaRpPTEgqmDXnSQgTNFEnQxcxF42oaMcb4";
const B: &str = "ULwVZmCfFXz4AhuiPmwJgBt873EuSHNE3VjpmVoPsaa";
const C: &str = "61pPQoBzka9SVZd664KCzAPHbEmwnhkb13V38mg1PUfy";

fn frame(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn ack(id: u64, sub: u64) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": sub }).to_string()
}

fn note(sub: u64, signature: &str, err: Value) -> String {
    json!({ "jsonrpc": "2.0", "method": "logsNotification", "params": { "subscription": sub, "result": { "context": { "slot": 1 }, "value": { "signature": signature, "err": err, "logs": [] } } } })
        .to_string()
}

/// Subscribes the given wallets and acknowledges each with sub id = 100 + its request id.
fn subscribed(book: &mut SubBook, wallets: &[&str]) {
    let want: Vec<String> = wallets.iter().map(|w| w.to_string()).collect();
    for sent in book.sync(&want) {
        let id = frame(&sent)["id"].as_u64().unwrap();
        assert_eq!(book.on_text(&ack(id, 100 + id)), (None, vec![]));
    }
}

#[test]
fn each_wallet_gets_its_own_subscription_at_the_configured_commitment() {
    let mut book = SubBook::new("finalized");
    let frames = book.sync(&[A.to_string(), B.to_string(), "not-an-address".to_string(), A.to_string()]);
    assert_eq!(frames.len(), 2, "one per valid wallet, duplicates and junk skipped");
    for sent in &frames {
        let sent = frame(sent);
        assert_eq!(sent["method"], "logsSubscribe");
        assert_eq!(sent["params"][0]["mentions"].as_array().unwrap().len(), 1, "mentions takes exactly one address");
        assert_eq!(sent["params"][1]["commitment"], "finalized");
    }
    assert_eq!(book.scope(), (2, 0), "wanted, but nothing confirmed yet");
    assert!(book.sync(&[A.to_string(), B.to_string()]).is_empty(), "nothing to change");
}

#[test]
fn a_notification_becomes_one_event_for_the_right_wallet() {
    let mut book = SubBook::new("confirmed");
    subscribed(&mut book, &[A, B]);
    assert_eq!(book.scope(), (2, 2));
    // sync sorts the fresh wallets, so A was request 1 (sub 101) and B request 2 (sub 102).
    let (first, second) = if A < B { (A, B) } else { (B, A) };
    assert_eq!(book.on_text(&note(101, "sig1", Value::Null)).0, Some((first.to_string(), "sig1".to_string())));
    assert_eq!(book.on_text(&note(102, "sig1", Value::Null)).0, Some((second.to_string(), "sig1".to_string())), "the same transaction can involve two watched wallets");
    assert_eq!(book.on_text(&note(101, "sig1", Value::Null)).0, None, "a repeat is dropped");
    assert_eq!(book.on_text(&note(101, "sig2", json!({ "InstructionError": [0, "Custom"] }))).0, None, "failed transactions are skipped");
    assert_eq!(book.on_text(&note(999, "sig3", Value::Null)).0, None, "an unknown subscription");
    assert_eq!(book.on_text("not json").0, None);
    assert_eq!(book.on_text(&json!({ "method": "slotNotification" }).to_string()).0, None);
}

#[test]
fn refresh_subscribes_and_unsubscribes_only_the_difference() {
    let mut book = SubBook::new("confirmed");
    subscribed(&mut book, &[A, B]);
    let frames: Vec<Value> = book.sync(&[B.to_string(), C.to_string()]).iter().map(|f| frame(f)).collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["method"], "logsUnsubscribe");
    let dropped_sub = frames[0]["params"][0].as_u64().unwrap();
    assert_eq!(frames[1]["method"], "logsSubscribe");
    assert_eq!(frames[1]["params"][0]["mentions"][0], C);
    assert_eq!(book.on_text(&note(dropped_sub, "late", Value::Null)).0, None, "a wallet that was removed no longer produces events");
    assert_eq!(book.scope(), (2, 1), "B confirmed, C in flight");
}

#[test]
fn a_wallet_removed_while_its_subscribe_is_in_flight_is_unsubscribed_on_the_ack() {
    let mut book = SubBook::new("confirmed");
    let sent = book.sync(&[A.to_string()]);
    let id = frame(&sent[0])["id"].as_u64().unwrap();
    assert!(book.sync(&[]).is_empty(), "no subscription id to unsubscribe yet");
    let (event, frames) = book.on_text(&ack(id, 77));
    assert_eq!(event, None);
    assert_eq!(frames.len(), 1);
    let unsub = frame(&frames[0]);
    assert_eq!((unsub["method"].as_str(), unsub["params"][0].as_u64()), (Some("logsUnsubscribe"), Some(77)));
    assert_eq!(book.scope(), (0, 0));
}

#[test]
fn a_refused_subscribe_can_be_asked_again() {
    let mut book = SubBook::new("confirmed");
    let sent = book.sync(&[A.to_string()]);
    let id = frame(&sent[0])["id"].as_u64().unwrap();
    book.on_text(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": "Invalid params" } }).to_string());
    assert_eq!(book.scope(), (0, 0));
    assert_eq!(book.sync(&[A.to_string()]).len(), 1, "the next refresh tries again");
}

#[test]
fn a_reconnect_rebuilds_subscriptions_without_replaying_signatures() {
    let mut book = SubBook::new("confirmed");
    subscribed(&mut book, &[A]);
    assert!(book.on_text(&note(101, "sig1", Value::Null)).0.is_some());

    book.reset();
    assert_eq!(book.scope(), (0, 0));
    let sent = book.sync(&[A.to_string()]);
    assert_eq!(sent.len(), 1, "a fresh socket needs every subscription again");
    let id = frame(&sent[0])["id"].as_u64().unwrap();
    book.on_text(&ack(id, 500));
    assert_eq!(book.on_text(&note(500, "sig1", Value::Null)).0, None, "already seen before the reconnect");
    assert!(book.on_text(&note(500, "sig2", Value::Null)).0.is_some());
    assert_eq!(book.on_text(&note(101, "sig3", Value::Null)).0, None, "the old socket's subscription id means nothing now");
}
