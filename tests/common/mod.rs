//! Shared by the integration tests: fixtures and fakes.
#![allow(dead_code)]

use regnerus::chain::solana::tx::ParsedTx;
use serde_json::Value;

pub const WALLET: &str = "FwySUYJVsjuaRpPTEgqmDXnSQgTNFEnQxcxF42oaMcb4";
pub const WALLET_2: &str = "ULwVZmCfFXz4AhuiPmwJgBt873EuSHNE3VjpmVoPsaa";
pub const OTHER: &str = "61pPQoBzka9SVZd664KCzAPHbEmwnhkb13V38mg1PUfy";
pub const MEME_MINT: &str = "2cHNNrQKqt4nvKFLB7iVUGA5UMngdgBJc1PGrtNh2c8W";
pub const MEME_MINT_2: &str = "H5kxvHgqsQabgjvuLPqBHWnLWteiLCuDY9ZyXpL1oe8z";
pub const T22_MINT: &str = "81D2onAVdAjWDNkUtHWtnuKBz9GdckLu1V6f2hjNNjws";
pub const PUMP: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const JUPITER: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
pub const RAYDIUM_AMM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const METEORA_DBC: &str = "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN";
pub const SYSTEM: &str = "11111111111111111111111111111111";
pub const CURVE: &str = "APRAfWrxwmpqaLU5eUockFECnrS8wbnLBQ9dTBgNa2uk";
pub const JITO_TIP: &str = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5";

pub const SIG_PUMP_BUY: &str = "5pumpBuyXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX1";
pub const SIG_JUP_SELL: &str = "5jupSellXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX2";
pub const SIG_SOL_TRANSFER: &str = "5solXferXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX3";
pub const SIG_TOKEN_OUT: &str = "5tokOutXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX4";
pub const SIG_T22_BUY: &str = "5t22BuyXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX5";

/// A fixture as the raw `getTransaction` result.
pub fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

pub fn parsed(value: &Value) -> ParsedTx {
    ParsedTx::from_value(value).expect("fixture parses")
}

pub const FIXTURE_NAMES: &[&str] = &["pump_buy", "jup_sell", "sol_transfer", "token_out", "t22_buy"];

// ---- fakes and the test harness ----

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;
use regnerus::app::App;
use regnerus::bot::telegram::{TelegramCall, TgReply};
use regnerus::chain::solana::labels::LabelBook;
use regnerus::chain::solana::rpc::{ChainRpc, RpcError, SignatureInfo};
use regnerus::config::Settings;
use regnerus::obs::budget::{Budget, BudgetConfig, Lane};
use regnerus::store::memory::MemoryStore;
use serde_json::json;

/// The block time of the first fixture; the harness clock starts 2 s later.
pub const T0: i64 = 1_759_400_000;
pub const ADMIN: i64 = 1;
pub const FRIEND: i64 = 2;
pub const STRANGER: i64 = 3;

#[derive(Default)]
pub struct FakeRpc {
    pub txs: Mutex<HashMap<String, Value>>,
    /// Per address, newest first, as a node returns them.
    pub sigs: Mutex<HashMap<String, Vec<SignatureInfo>>>,
    /// Signatures the node answers `null` for this many more times.
    pub not_yet: Mutex<HashMap<String, u32>>,
    pub broken: Mutex<HashSet<String>>,
    pub calls: Mutex<Vec<String>>,
    /// Answers to other methods, keyed `method:first-param`, or `method` alone.
    pub raw: Mutex<HashMap<String, Value>>,
}

impl FakeRpc {
    pub fn put(&self, tx: &Value) -> String {
        let signature = tx["transaction"]["signatures"][0].as_str().unwrap().to_string();
        self.txs.lock().insert(signature.clone(), tx.clone());
        signature
    }

    /// Makes `signature` the newest entry in an address's history.
    pub fn push_sig(&self, address: &str, signature: &str, failed: bool) {
        self.sigs.lock().entry(address.to_string()).or_default().insert(0, SignatureInfo { signature: signature.to_string(), slot: 0, failed, block_time: None });
    }

    pub fn fetches(&self) -> Vec<String> {
        self.calls.lock().iter().filter(|c| c.starts_with("tx:")).cloned().collect()
    }
}

#[async_trait]
impl ChainRpc for FakeRpc {
    async fn get_transaction(&self, signature: &str, lane: Lane) -> Result<Option<Value>, RpcError> {
        self.calls.lock().push(format!("tx:{signature}:{}", lane.as_str()));
        if self.broken.lock().contains(signature) {
            return Err(RpcError::Http(503));
        }
        if let Some(left) = self.not_yet.lock().get_mut(signature)
            && *left > 0
        {
            *left -= 1;
            return Ok(None);
        }
        Ok(self.txs.lock().get(signature).cloned())
    }

    async fn get_signatures(&self, address: &str, until: Option<&str>, limit: usize, lane: Lane) -> Result<Vec<SignatureInfo>, RpcError> {
        self.calls.lock().push(format!("sigs:{address}:{}:{}", until.unwrap_or("-"), lane.as_str()));
        let all = self.sigs.lock().get(address).cloned().unwrap_or_default();
        Ok(all.into_iter().take_while(|row| Some(row.signature.as_str()) != until).take(limit).collect())
    }

    async fn raw(&self, method: &str, params: Value, lane: Lane) -> Result<Value, RpcError> {
        let first = params.get(0).map(|p| p.as_str().map(str::to_string).unwrap_or_else(|| p.to_string())).unwrap_or_default();
        self.calls.lock().push(format!("raw:{method}:{first}:{}", lane.as_str()));
        if method == "getSignaturesForAddress" {
            // Paging backwards: everything older than `before`.
            let before = params.pointer("/1/before").and_then(Value::as_str);
            let limit = params.pointer("/1/limit").and_then(Value::as_u64).unwrap_or(1000) as usize;
            let all = self.sigs.lock().get(&first).cloned().unwrap_or_default();
            let start = before.and_then(|b| all.iter().position(|row| row.signature == b).map(|i| i + 1)).unwrap_or(0);
            let rows: Vec<Value> = all
                .iter()
                .skip(start)
                .take(limit)
                .map(|row| json!({ "signature": row.signature, "slot": row.slot, "err": if row.failed { json!({}) } else { Value::Null }, "blockTime": row.block_time }))
                .collect();
            return Ok(json!(rows));
        }
        let raw = self.raw.lock();
        Ok(raw.get(&format!("{method}:{first}")).or_else(|| raw.get(method)).cloned().unwrap_or(Value::Null))
    }
}

#[derive(Default)]
pub struct FakeTelegram {
    pub calls: Mutex<Vec<(String, Value)>>,
    next_id: AtomicI64,
    /// Chats whose sends fail, as when a user has blocked the bot.
    pub blocked: Mutex<HashSet<i64>>,
}

impl FakeTelegram {
    fn of(&self, method: &str) -> Vec<Value> {
        self.calls.lock().iter().filter(|(m, _)| m == method).map(|(_, p)| p.clone()).collect()
    }

    /// `(chat, text)` of every message sent, in order.
    pub fn sent(&self) -> Vec<(i64, String)> {
        self.of("sendMessage").iter().map(|p| (p["chat_id"].as_i64().unwrap(), p["text"].as_str().unwrap().to_string())).collect()
    }

    pub fn sent_to(&self, chat: i64) -> Vec<String> {
        self.sent().into_iter().filter(|(c, _)| *c == chat).map(|(_, t)| t).collect()
    }

    pub fn last_to(&self, chat: i64) -> String {
        self.sent_to(chat).pop().unwrap_or_default()
    }

    /// `(chat, message_id, text)` of every edit, in order.
    pub fn edits(&self) -> Vec<(i64, i64, String)> {
        self.of("editMessageText").iter().map(|p| (p["chat_id"].as_i64().unwrap(), p["message_id"].as_i64().unwrap(), p["text"].as_str().unwrap().to_string())).collect()
    }

    pub fn toasts(&self) -> Vec<String> {
        self.of("answerCallbackQuery").iter().map(|p| p["text"].as_str().unwrap_or("").to_string()).collect()
    }

    /// The buttons under the last message sent to a chat: `(text, callback data or url)`.
    pub fn last_buttons(&self, chat: i64) -> Vec<(String, String)> {
        let sends = self.of("sendMessage");
        let Some(last) = sends.iter().rev().find(|p| p["chat_id"].as_i64() == Some(chat)) else { return Vec::new() };
        last["reply_markup"]["inline_keyboard"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|row| row.as_array().cloned().unwrap_or_default())
            .map(|b| (b["text"].as_str().unwrap_or("").to_string(), b["callback_data"].as_str().or(b["url"].as_str()).unwrap_or("").to_string()))
            .collect()
    }

    pub fn clear(&self) {
        self.calls.lock().clear();
    }
}

#[async_trait]
impl TelegramCall for FakeTelegram {
    async fn call(&self, method: &str, payload: Value, _long_poll: bool) -> TgReply {
        self.calls.lock().push((method.to_string(), payload.clone()));
        match method {
            "sendMessage" => {
                if self.blocked.lock().contains(&payload["chat_id"].as_i64().unwrap_or(0)) {
                    return TgReply::failed("Forbidden: bot was blocked by the user");
                }
                TgReply::ok(json!({ "message_id": self.next_id.fetch_add(1, Ordering::Relaxed) + 1 }))
            }
            _ => TgReply::ok(json!({})),
        }
    }
}

pub struct Harness {
    pub app: Arc<App>,
    pub rpc: Arc<FakeRpc>,
    pub tg: Arc<FakeTelegram>,
    pub store: Arc<MemoryStore>,
    pub now: Arc<AtomicI64>,
}

impl Harness {
    /// Sends a private message as a user and returns the bot's replies to it.
    pub async fn say(&self, user: i64, text: &str) -> Vec<String> {
        let before = self.tg.sent_to(user).len();
        self.app.on_update(&message(user, text)).await;
        settle().await;
        self.tg.sent_to(user).split_off(before)
    }

    pub async fn press(&self, user: i64, data: &str, message_id: i64) {
        self.app
            .on_update(
                &json!({ "update_id": 1, "callback_query": { "id": "cb1", "from": { "id": user }, "data": data, "message": { "message_id": message_id, "chat": { "id": user, "type": "private" } } } }),
            )
            .await;
        settle().await;
    }

    pub fn advance(&self, seconds: i64) {
        self.now.fetch_add(seconds, Ordering::Relaxed);
    }
}

/// Lets background tasks started by a command run to completion.
pub async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
}

pub fn message(user: i64, text: &str) -> Value {
    json!({ "update_id": 1, "message": { "message_id": 10, "from": { "id": user, "first_name": format!("user{user}"), "username": format!("u{user}") }, "chat": { "id": user, "type": "private" }, "text": text } })
}

pub fn harness_with(extra: &[(&str, &str)]) -> Harness {
    let mut env: HashMap<String, String> =
        [("ADMIN_IDS", "1"), ("SOLANA_RPC_URL", "https://rpc.example/key"), ("TELEGRAM_BOT_TOKEN", "t")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    let settings = Settings::from_lookup(&|name| env.get(name).cloned());
    let now = Arc::new(AtomicI64::new(T0 + 2));
    let clock_now = now.clone();
    let clock: regnerus::util::Clock = Arc::new(move || clock_now.load(Ordering::Relaxed));
    let rpc = Arc::new(FakeRpc::default());
    let tg = Arc::new(FakeTelegram::default());
    let store = Arc::new(MemoryStore::new());
    let budget = Arc::new(Budget::new(BudgetConfig { requests_per_min: 60_000.0, burst: 1_000.0, daily_credits: 1_000_000, forensics_share: 0.3 }, clock.clone()));
    let app = App::new(settings, store.clone(), rpc.clone(), tg.clone(), Arc::new(LabelBook::seeded()), budget, clock, "rpc.example".to_string());
    Harness { app, rpc, tg, store, now }
}

pub fn harness() -> Harness {
    harness_with(&[])
}

/// A fixture rewritten as if another wallet had sent it, later.
pub fn as_wallet(name: &str, wallet: &str, signature: &str, block_time: i64) -> Value {
    let text = serde_json::to_string(&fixture(name)).unwrap().replace(WALLET, wallet);
    let mut tx: Value = serde_json::from_str(&text).unwrap();
    tx["transaction"]["signatures"][0] = json!(signature);
    tx["blockTime"] = json!(block_time);
    tx
}

// ---- helpers for the trail, cluster, trader and reviewer tests ----

use regnerus::bot::telegram::Card;
use regnerus::model::{Label, LabelClass};
use regnerus::review::{Review, ReviewVerdict, Reviewer};
use regnerus::trade::keys::KeyProvider;
use regnerus::trade::venue::{Fill, Order, OrderRequest, SwapVenue, VenueError};

pub const SOL: u64 = 1_000_000_000;
pub const WSOL: &str = "So11111111111111111111111111111111111111112";

/// A made-up but well-formed address: 32 bytes of `n`, with `tag` in front.
pub fn addr(n: u8) -> String {
    let mut bytes = [n; 32];
    bytes[0] = 200;
    bytes[1] = n;
    bs58::encode(bytes).into_string()
}

/// A plain SOL transfer, as `getTransaction` returns it.
pub fn sol_tx(from: &str, to: &str, lamports: u64, signature: &str, block_time: i64) -> Value {
    json!({
        "slot": 300_000_000u64 + (block_time - T0).max(0) as u64 * 2,
        "blockTime": block_time,
        "version": "legacy",
        "transaction": {
            "signatures": [signature],
            "message": {
                "accountKeys": [
                    { "pubkey": from, "signer": true, "writable": true, "source": "transaction" },
                    { "pubkey": to, "signer": false, "writable": true, "source": "transaction" },
                    { "pubkey": SYSTEM, "signer": false, "writable": false, "source": "transaction" }
                ],
                "instructions": [{ "program": "system", "programId": SYSTEM, "parsed": { "type": "transfer", "info": { "source": from, "destination": to, "lamports": lamports } } }]
            }
        },
        "meta": { "err": null, "fee": 5000, "preBalances": [lamports + 1_000_000_000, 0, 1], "postBalances": [1_000_000_000 - 5000, lamports, 1], "preTokenBalances": [], "postTokenBalances": [], "innerInstructions": [], "logMessages": [] }
    })
}

impl FakeRpc {
    /// Stores a transfer and lists it, newest first, in both wallets' histories.
    pub fn transfer(&self, from: &str, to: &str, lamports: u64, signature: &str, block_time: i64) {
        self.put(&sol_tx(from, to, lamports, signature, block_time));
        for address in [from, to] {
            self.sigs.lock().entry(address.to_string()).or_default().insert(0, SignatureInfo { signature: signature.to_string(), slot: 0, failed: false, block_time: Some(block_time) });
        }
    }
}

impl Harness {
    pub fn label(&self, address: &str, class: LabelClass, name: &str) {
        self.app.labels.set(Label { address: address.to_string(), class, name: name.to_string(), source: "test".to_string() });
    }

    /// Runs every queued job that is due, and whatever those queue in turn.
    pub async fn work(&self) -> usize {
        let mut ran = 0;
        loop {
            let pass = self.app.drain_jobs(50).await;
            settle().await;
            if pass == 0 {
                return ran;
            }
            ran += pass;
        }
    }

    /// Delivers a transaction to the pipeline as the live stream would.
    pub async fn see(&self, wallet: &str, tx: &Value) -> regnerus::pipeline::Outcome {
        let signature = self.rpc.put(tx);
        let outcome = self.app.process_signature(wallet, &signature, true, false).await;
        settle().await;
        outcome
    }

    /// Everything sent to a chat since the last `clear`, joined.
    pub fn inbox(&self, chat: i64) -> String {
        self.tg.sent_to(chat).join("\n---\n")
    }
}

/// Builds the base64 transaction a fake venue returns for an order.
pub type TxBuilder = Box<dyn Fn(&OrderRequest) -> String + Send>;

/// A swap venue that fills at a set price per token.
pub struct FakeVenue {
    /// Lamports per raw token unit, per mint. A mint with no price has no route.
    pub prices: Mutex<HashMap<String, f64>>,
    pub impact_pct: Mutex<f64>,
    pub orders: Mutex<Vec<OrderRequest>>,
    pub executed: Mutex<Vec<(String, String)>>,
    /// Builds the transaction for an order with a taker. `None`: none is returned.
    pub build: Mutex<Option<TxBuilder>>,
    pub fail_execute: Mutex<Option<VenueError>>,
    pub gasless: Mutex<bool>,
}

impl Default for FakeVenue {
    fn default() -> Self {
        FakeVenue {
            prices: Mutex::new(HashMap::new()),
            impact_pct: Mutex::new(0.1),
            orders: Mutex::new(Vec::new()),
            executed: Mutex::new(Vec::new()),
            build: Mutex::new(None),
            fail_execute: Mutex::new(None),
            gasless: Mutex::new(false),
        }
    }
}

impl FakeVenue {
    pub fn price(&self, mint: &str, lamports_per_unit: f64) {
        self.prices.lock().insert(mint.to_string(), lamports_per_unit);
    }

    pub fn no_route(&self, mint: &str) {
        self.prices.lock().remove(mint);
    }
}

#[async_trait]
impl SwapVenue for FakeVenue {
    async fn order(&self, request: &OrderRequest) -> Result<Order, VenueError> {
        self.orders.lock().push(request.clone());
        let buying = request.input_mint == WSOL;
        let token = if buying { &request.output_mint } else { &request.input_mint };
        let Some(price) = self.prices.lock().get(token).copied() else { return Err(VenueError::new("no_route", "no route for this token right now")) };
        let out = if buying { (request.amount as f64 / price) as u64 } else { (request.amount as f64 * price) as u64 };
        let transaction = match (&request.taker, self.build.lock().as_ref()) {
            (Some(_), Some(build)) => Some(build(request)),
            _ => None,
        };
        if request.taker.is_some() && transaction.is_none() {
            return Err(VenueError::new("order_1", "insufficient funds"));
        }
        Ok(Order {
            in_amount: request.amount,
            out_amount: out,
            price_impact_pct: *self.impact_pct.lock(),
            transaction,
            request_id: format!("req{}", self.orders.lock().len()),
            gasless: *self.gasless.lock(),
            router: "metis".into(),
        })
    }

    async fn execute(&self, signed_transaction: &str, request_id: &str) -> Result<Fill, VenueError> {
        self.executed.lock().push((signed_transaction.to_string(), request_id.to_string()));
        if let Some(error) = self.fail_execute.lock().clone() {
            return Err(error);
        }
        // Filled exactly as the matching order was quoted.
        let index: usize = request_id.trim_start_matches("req").parse::<usize>().unwrap_or(1) - 1;
        let request = self.orders.lock()[index].clone();
        let buying = request.input_mint == WSOL;
        let token = if buying { &request.output_mint } else { &request.input_mint };
        let price = self.prices.lock().get(token).copied().unwrap_or(1.0);
        let out = if buying { (request.amount as f64 / price) as u64 } else { (request.amount as f64 * price) as u64 };
        Ok(Fill { success: true, signature: Some(format!("sigFill{}", index + 1)), code: 0, error: None, input: request.amount, output: out })
    }

    fn name(&self) -> String {
        "fake-venue".into()
    }
}

/// A key provider that holds its secrets in memory.
#[derive(Default)]
pub struct FakeKeys {
    pub secrets: Mutex<HashMap<String, String>>,
    pub can_store: bool,
}

#[async_trait]
impl KeyProvider for FakeKeys {
    async fn reveal(&self, reference: &str) -> Result<String, String> {
        self.secrets.lock().get(reference).cloned().ok_or_else(|| format!("secret {reference} is not set"))
    }

    async fn put(&self, reference: &str, value: &str) -> Result<(), String> {
        if !self.can_store {
            return Err("this provider cannot store keys".into());
        }
        self.secrets.lock().insert(reference.to_string(), value.to_string());
        Ok(())
    }

    fn can_store(&self) -> bool {
        self.can_store
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}

/// A reviewer that answers what the test told it to.
pub struct FakeReviewer {
    pub verdict: Mutex<Result<ReviewVerdict, String>>,
    pub cases: Mutex<Vec<Value>>,
}

impl FakeReviewer {
    pub fn saying(verdict: ReviewVerdict) -> Self {
        FakeReviewer { verdict: Mutex::new(Ok(verdict)), cases: Mutex::new(Vec::new()) }
    }
}

#[async_trait]
impl Reviewer for FakeReviewer {
    async fn review(&self, case_file: &Value) -> Result<Review, String> {
        self.cases.lock().push(case_file.clone());
        let verdict = self.verdict.lock().clone()?;
        Ok(Review { verdict, confidence: 0.8, reasons: vec!["E1: funded directly".into()], would_settle: vec!["a deposit to the same exchange address".into()] })
    }

    fn model(&self) -> String {
        "fake-model".into()
    }
}

pub fn text_of(card: &Card) -> &str {
    &card.text
}
