//! The real network code against local servers: the JSON-RPC client over
//! HTTP, the websocket stream, and the whole running bot (`serve`) wired the
//! way `main` wires it. Only Telegram is a fake here.

mod common;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::*;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use regnerus::app::App;
use regnerus::bot::telegram::{TelegramCall, TgReply};
use regnerus::chain::solana::labels::LabelBook;
use regnerus::chain::solana::rpc::{ChainRpc, Commitment, RpcError, SolanaRpc};
use regnerus::config::Settings;
use regnerus::obs::budget::{Budget, BudgetConfig, Lane};
use regnerus::runtime::serve;
use regnerus::store::Store;
use regnerus::store::memory::MemoryStore;
use regnerus::util::system_clock;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio_tungstenite::tungstenite::Message;

fn tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// ---- a JSON-RPC node over plain HTTP ----

enum Scripted {
    Status(u16, Vec<(&'static str, String)>),
    Body(Value),
}

#[derive(Default)]
struct NodeState {
    requests: Vec<Value>,
    script: VecDeque<Scripted>,
    txs: HashMap<String, Value>,
    /// Per address, newest first.
    sigs: HashMap<String, Vec<String>>,
}

struct MockNode {
    addr: SocketAddr,
    state: Arc<Mutex<NodeState>>,
}

impl MockNode {
    async fn start() -> MockNode {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(NodeState::default()));
        let shared = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let state = shared.clone();
                tokio::spawn(async move {
                    let mut buffer: Vec<u8> = Vec::new();
                    loop {
                        // Read one request: headers, then Content-Length bytes of body.
                        let header_end = loop {
                            if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                                break at + 4;
                            }
                            let mut chunk = [0u8; 4096];
                            match socket.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                            }
                        };
                        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
                        let length: usize = headers.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                        while buffer.len() < header_end + length {
                            let mut chunk = [0u8; 4096];
                            match socket.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let request: Value = serde_json::from_slice(&buffer[header_end..header_end + length]).unwrap_or(Value::Null);
                        buffer.drain(..header_end + length);

                        let reply = {
                            let mut state = state.lock();
                            state.requests.push(request.clone());
                            match state.script.pop_front() {
                                Some(scripted) => scripted,
                                None => Scripted::Body(json!({ "jsonrpc": "2.0", "id": request["id"], "result": answer(&state, &request) })),
                            }
                        };
                        let response = match reply {
                            Scripted::Status(status, headers) => {
                                let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                                format!("HTTP/1.1 {status} X\r\n{extra}content-length: 0\r\n\r\n")
                            }
                            Scripted::Body(body) => {
                                let body = body.to_string();
                                format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len())
                            }
                        };
                        if socket.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        MockNode { addr, state }
    }

    /// The path stands in for an API key: it must never be logged or shown.
    fn url(&self) -> String {
        format!("http://{}/v2/SECRETKEY", self.addr)
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        self.state.lock().requests.iter().filter(|r| r["method"] == method).cloned().collect()
    }
}

/// What an honest node answers.
fn answer(state: &NodeState, request: &Value) -> Value {
    match request["method"].as_str() {
        Some("getTransaction") => state.txs.get(request["params"][0].as_str().unwrap_or("")).cloned().unwrap_or(Value::Null),
        Some("getSignaturesForAddress") => {
            let until = request["params"][1]["until"].as_str();
            let all = state.sigs.get(request["params"][0].as_str().unwrap_or("")).cloned().unwrap_or_default();
            json!(all.iter().take_while(|s| Some(s.as_str()) != until).map(|s| json!({ "signature": s, "slot": 1, "err": null, "blockTime": null })).collect::<Vec<_>>())
        }
        _ => Value::Null,
    }
}

fn client(node: &MockNode) -> (SolanaRpc, Arc<Budget>) {
    tls_provider();
    let budget = Arc::new(Budget::new(BudgetConfig { requests_per_min: 6_000.0, burst: 100.0, daily_credits: 1_000_000, forensics_share: 0.3 }, system_clock()));
    (SolanaRpc::new(&node.url(), Commitment::Finalized, budget.clone()).unwrap(), budget)
}

#[tokio::test]
async fn the_rpc_client_asks_for_parsed_transactions_at_the_configured_commitment() {
    let node = MockNode::start().await;
    let (rpc, budget) = client(&node);
    let tx = fixture("pump_buy");
    node.state.lock().txs.insert(SIG_PUMP_BUY.to_string(), tx.clone());
    node.state.lock().sigs.insert(WALLET.to_string(), vec!["s3".into(), "s2".into(), "s1".into()]);

    assert_eq!(rpc.get_transaction(SIG_PUMP_BUY, Lane::Copy).await.unwrap(), Some(tx));
    assert_eq!(rpc.get_transaction("unknownSignature", Lane::Watch).await.unwrap(), None, "a null result is 'not there yet', not an error");
    let request = &node.requests("getTransaction")[0];
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["params"], json!([SIG_PUMP_BUY, { "encoding": "jsonParsed", "commitment": "finalized", "maxSupportedTransactionVersion": 0 }]));

    let rows = rpc.get_signatures(WALLET, Some("s1"), 25, Lane::Watch).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.signature.as_str()).collect::<Vec<_>>(), vec!["s3", "s2"]);
    assert!(!rows[0].failed);
    assert_eq!(node.requests("getSignaturesForAddress")[0]["params"], json!([WALLET, { "limit": 25, "commitment": "finalized", "until": "s1" }]));
    assert_eq!(budget.snapshot().spent, [1, 2, 0], "each call is one credit on its own lane");
}

#[tokio::test]
async fn a_429_is_waited_out_and_the_call_then_succeeds() {
    let node = MockNode::start().await;
    let (rpc, budget) = client(&node);
    node.state.lock().txs.insert("sig".to_string(), json!({ "slot": 5 }));
    node.state.lock().script.push_back(Scripted::Status(429, vec![("retry-after", "0.2".to_string())]));

    let started = Instant::now();
    assert_eq!(rpc.get_transaction("sig", Lane::Copy).await.unwrap(), Some(json!({ "slot": 5 })));
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(1_100), "Retry-After plus the margin: {waited:?}");
    assert_eq!(node.requests("getTransaction").len(), 2);
    let snap = budget.snapshot();
    assert_eq!(snap.rate_limits, 1);
    assert!(snap.rate_per_min < snap.max_rate, "the rate was cut");
}

#[tokio::test]
async fn rpc_errors_come_back_as_errors_without_the_key() {
    let node = MockNode::start().await;
    let (rpc, _) = client(&node);
    node.state.lock().script.push_back(Scripted::Body(json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "Invalid param: WrongSize" } })));
    assert_eq!(rpc.get_transaction("x", Lane::Watch).await, Err(RpcError::Rpc { code: -32602, message: "Invalid param: WrongSize".into() }));
    assert_eq!(node.requests("getTransaction").len(), 1, "a refusal is not retried");

    // A node-side rate-limit error is retried like a 429.
    node.state.lock().script.push_back(Scripted::Body(json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32429, "message": "rate limited" } })));
    for _ in 0..3 {
        node.state.lock().script.push_back(Scripted::Status(503, vec![]));
    }
    // One -32429, then three 503s: the first call uses three attempts and gives up.
    assert_eq!(rpc.get_transaction("x", Lane::Watch).await, Err(RpcError::Http(503)));
    node.state.lock().script.clear();

    // Nothing listening: a transport error that names the host, never the path.
    tls_provider();
    let budget = Arc::new(Budget::new(BudgetConfig::default(), system_clock()));
    let dead = SolanaRpc::new("http://127.0.0.1:9/v2/SECRETKEY", Commitment::Finalized, budget).unwrap();
    let error = dead.get_transaction("x", Lane::Watch).await.unwrap_err().to_string();
    assert!(error.contains("could not reach the RPC"), "{error}");
    assert!(!error.contains("SECRETKEY"), "{error}");
    assert_eq!(dead.host(), "127.0.0.1:9");
}

// ---- a websocket node ----

#[derive(Clone, Debug)]
enum WsCommand {
    Notify { address: String, signature: String },
    Drop,
}

struct MockWs {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<Value>>>,
    connections: Arc<AtomicUsize>,
    commands: broadcast::Sender<WsCommand>,
}

impl MockWs {
    async fn start() -> MockWs {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let (commands, _) = broadcast::channel(64);
        let (shared, count, sender) = (received.clone(), connections.clone(), commands.clone());
        tokio::spawn(async move {
            let mut next_sub = 100u64;
            loop {
                let Ok((socket, _)) = listener.accept().await else { return };
                let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else { continue };
                count.fetch_add(1, Ordering::SeqCst);
                let mut commands = sender.subscribe();
                let mut subs: HashMap<String, u64> = HashMap::new();
                loop {
                    tokio::select! {
                        frame = ws.next() => match frame {
                            Some(Ok(Message::Text(text))) => {
                                let request: Value = serde_json::from_str(text.as_str()).unwrap();
                                shared.lock().push(request.clone());
                                let result = match request["method"].as_str() {
                                    Some("logsSubscribe") => {
                                        next_sub += 1;
                                        subs.insert(request["params"][0]["mentions"][0].as_str().unwrap().to_string(), next_sub);
                                        json!(next_sub)
                                    }
                                    _ => json!(true),
                                };
                                let _ = ws.send(Message::text(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }).to_string())).await;
                            }
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                        command = commands.recv() => match command {
                            Ok(WsCommand::Notify { address, signature }) => {
                                if let Some(sub) = subs.get(&address) {
                                    let note = json!({ "jsonrpc": "2.0", "method": "logsNotification", "params": { "subscription": sub, "result": { "context": { "slot": 9 }, "value": { "signature": signature, "err": null, "logs": [] } } } });
                                    let _ = ws.send(Message::text(note.to_string())).await;
                                }
                            }
                            Ok(WsCommand::Drop) => break,
                            Err(_) => {}
                        },
                    }
                }
                // Falling out of the loop drops the socket: the client sees the connection die.
            }
        });
        MockWs { addr, received, connections, commands }
    }

    fn subscribed(&self) -> Vec<String> {
        self.received.lock().iter().filter(|r| r["method"] == "logsSubscribe").map(|r| r["params"][0]["mentions"][0].as_str().unwrap().to_string()).collect()
    }
}

// ---- Telegram with a queue of incoming updates ----

#[derive(Default)]
struct ScriptedTelegram {
    incoming: Mutex<VecDeque<Value>>,
    sent: Mutex<Vec<(i64, String)>>,
    next_update: AtomicUsize,
}

impl ScriptedTelegram {
    fn user_says(&self, user: i64, text: &str) {
        let mut update = message(user, text);
        update["update_id"] = json!(self.next_update.fetch_add(1, Ordering::SeqCst) + 1);
        self.incoming.lock().push_back(update);
    }

    fn texts(&self) -> Vec<String> {
        self.sent.lock().iter().map(|(_, t)| t.clone()).collect()
    }
}

#[async_trait]
impl TelegramCall for ScriptedTelegram {
    async fn call(&self, method: &str, payload: Value, _long_poll: bool) -> TgReply {
        match method {
            "getUpdates" => {
                let next = self.incoming.lock().pop_front();
                match next {
                    Some(update) => TgReply::ok(json!([update])),
                    None => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        TgReply::ok(json!([]))
                    }
                }
            }
            "sendMessage" => {
                let mut sent = self.sent.lock();
                sent.push((payload["chat_id"].as_i64().unwrap_or(0), payload["text"].as_str().unwrap_or("").to_string()));
                TgReply::ok(json!({ "message_id": sent.len() }))
            }
            _ => TgReply::ok(json!({})),
        }
    }
}

/// Polls until `check` is true, or fails with `what` after five seconds.
async fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn unix_now() -> i64 {
    system_clock()()
}

#[tokio::test]
async fn the_running_bot_tracks_subscribes_alerts_and_recovers_from_a_dropped_stream() {
    tls_provider();
    let node = MockNode::start().await;
    let ws = MockWs::start().await;
    let env: HashMap<&str, String> =
        [("ADMIN_IDS", "1".to_string()), ("TELEGRAM_BOT_TOKEN", "t".to_string()), ("SOLANA_RPC_URL", node.url()), ("SOLANA_WS_URL", format!("ws://{}/v2/SECRETKEY", ws.addr))].into_iter().collect();
    let settings = Settings::from_lookup(&|name| env.get(name).cloned());
    let budget = Arc::new(Budget::new(BudgetConfig::default(), system_clock()));
    let rpc = SolanaRpc::new(&settings.solana_rpc_url, settings.commitment, budget.clone()).unwrap();
    let host = rpc.host().to_string();
    let tg = Arc::new(ScriptedTelegram::default());
    let store = Arc::new(MemoryStore::new());
    let app = App::new(settings, store.clone(), Arc::new(rpc), tg.clone(), Arc::new(LabelBook::seeded()), budget, system_clock(), host);
    let (stop, stopped) = watch::channel(false);
    let running = tokio::spawn(serve(app.clone(), stopped));

    // It connects with nothing to watch yet.
    eventually("the first connect", || ws.connections.load(Ordering::SeqCst) == 1).await;
    assert!(ws.subscribed().is_empty());

    // An admin tracks a wallet: the reply arrives and the stream subscribes to it.
    tg.user_says(ADMIN, &format!("/track {WALLET} whale"));
    eventually("the tracking reply", || tg.texts().iter().any(|t| t.contains("Tracking started"))).await;
    eventually("the subscription", || ws.subscribed() == vec![WALLET.to_string()]).await;
    eventually("the history read", || !node.requests("getSignaturesForAddress").is_empty()).await;

    // The wallet buys. The node pushes the signature; the bot fetches, classifies and alerts.
    let buy = as_wallet("pump_buy", WALLET, "sigLiveBuy", unix_now() - 1);
    node.state.lock().txs.insert("sigLiveBuy".to_string(), buy);
    eventually("the subscribe ack to be applied", || app.health.subscriptions.load(Ordering::Relaxed) == 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    ws.commands.send(WsCommand::Notify { address: WALLET.to_string(), signature: "sigLiveBuy".to_string() }).unwrap();
    eventually("the buy card", || tg.texts().iter().any(|t| t.contains("<b>BUY</b> · <b>E#1 whale</b>"))).await;
    assert_eq!(store.counts().await.unwrap().trades, 1);
    assert!(app.health.connected.load(Ordering::Relaxed));

    // The stream dies. A sell happens while it is down. On reconnect the bot
    // subscribes again and the backfill finds the sell.
    let sell = as_wallet("jup_sell", WALLET, "sigWhileDown", unix_now());
    node.state.lock().txs.insert("sigWhileDown".to_string(), sell);
    node.state.lock().sigs.insert(WALLET.to_string(), vec!["sigWhileDown".into(), "sigLiveBuy".into()]);
    ws.commands.send(WsCommand::Drop).unwrap();
    eventually("the reconnect", || ws.connections.load(Ordering::SeqCst) == 2).await;
    eventually("the sell card from the backfill", || tg.texts().iter().any(|t| t.contains("<b>SELL</b> · <b>E#1 whale</b>"))).await;
    assert_eq!(ws.subscribed(), vec![WALLET.to_string(), WALLET.to_string()], "subscribed again on the new socket");
    assert_eq!(node.requests("getTransaction").iter().filter(|r| r["params"][0] == "sigLiveBuy").count(), 1, "the buy was not fetched a second time");
    assert_eq!(tg.texts().iter().filter(|t| t.contains("<b>BUY</b>")).count(), 1);

    // Nothing the bot said carries the key from the RPC URL.
    tg.user_says(ADMIN, "/health");
    eventually("the health card", || tg.texts().iter().any(|t| t.contains("<b>Health</b>"))).await;
    let health = tg.texts().into_iter().find(|t| t.contains("<b>Health</b>")).unwrap();
    assert!(health.contains("Stream: connected · 1 wallet · 2 connects"), "{health}");
    assert!(tg.texts().iter().all(|t| !t.contains("SECRETKEY")));

    // It stops when told to.
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), running).await.expect("serve stops promptly").unwrap();
    assert!(!app.health.connected.load(Ordering::Relaxed));
}

// ---- the other HTTP clients: the swap venue, the reviewer, the key provider ----

use regnerus::review::{AnthropicReviewer, ReviewVerdict, Reviewer};
use regnerus::trade::keys::{DopplerKeys, KeyProvider};
use regnerus::trade::venue::{JupiterVenue, OrderRequest, SwapVenue};

#[derive(Clone, Debug, Default)]
struct Seen {
    method: String,
    /// Path with its query string.
    target: String,
    headers: HashMap<String, String>,
    body: String,
}

/// An HTTP server that records each request and answers from a script.
struct MockHttp {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    replies: Arc<Mutex<VecDeque<(u16, String)>>>,
}

impl MockHttp {
    async fn start() -> MockHttp {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let replies: Arc<Mutex<VecDeque<(u16, String)>>> = Arc::default();
        let (seen_in, replies_in) = (seen.clone(), replies.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let (seen, replies) = (seen_in.clone(), replies_in.clone());
                tokio::spawn(async move {
                    let mut buffer: Vec<u8> = Vec::new();
                    loop {
                        let header_end = loop {
                            if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                                break at + 4;
                            }
                            let mut chunk = [0u8; 4096];
                            match socket.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                        let mut lines = head.lines();
                        let mut first = lines.next().unwrap_or("").split(' ');
                        let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
                        let headers: HashMap<String, String> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
                        let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                        while buffer.len() < header_end + length {
                            let mut chunk = [0u8; 4096];
                            match socket.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let body = String::from_utf8_lossy(&buffer[header_end..header_end + length]).to_string();
                        buffer.drain(..header_end + length);
                        seen.lock().push(Seen { method, target, headers, body });
                        let (status, body) = replies.lock().pop_front().unwrap_or((200, "{}".to_string()));
                        let response = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                        if socket.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        MockHttp { addr, seen, replies }
    }

    fn reply(&self, status: u16, body: Value) {
        self.replies.lock().push_back((status, body.to_string()));
    }

    fn last(&self) -> Seen {
        self.seen.lock().last().cloned().expect("a request was made")
    }
}

#[tokio::test]
async fn the_swap_venue_asks_for_quotes_and_orders_and_reads_the_fill() {
    tls_provider();
    let server = MockHttp::start().await;
    let venue = JupiterVenue::new(&format!("http://{}/swap/v2", server.addr), "KEY123", "jupiterz,dflow,okx").unwrap();
    let wsol = "So11111111111111111111111111111111111111112";

    // A quote: no taker, so no transaction.
    server.reply(200, json!({ "inAmount": "50000000", "outAmount": "1600000", "priceImpact": -0.42, "transaction": null, "requestId": "r1", "gasless": false, "router": "metis" }));
    let quote = venue.order(&OrderRequest { input_mint: wsol.into(), output_mint: MEME_MINT.into(), amount: 50_000_000, taker: None, slippage_bps: None }).await.unwrap();
    assert_eq!(
        (quote.in_amount, quote.out_amount, quote.price_impact_pct, quote.transaction.clone(), quote.request_id.as_str(), quote.router.as_str()),
        (50_000_000, 1_600_000, -0.42, None, "r1", "metis")
    );
    let asked = server.last();
    assert_eq!(asked.method, "GET");
    assert_eq!(asked.target, format!("/swap/v2/order?inputMint={wsol}&outputMint={MEME_MINT}&amount=50000000&excludeRouters=jupiterz%2Cdflow%2Cokx"));
    assert_eq!(asked.headers.get("x-api-key").map(String::as_str), Some("KEY123"));

    // An order: the taker and the slippage go in, a transaction comes back.
    server.reply(200, json!({ "inAmount": "50000000", "outAmount": "1600000", "priceImpactPct": "0.0042", "transaction": "AQID", "requestId": "r2", "gasless": true, "router": "metis" }));
    let order = venue.order(&OrderRequest { input_mint: wsol.into(), output_mint: MEME_MINT.into(), amount: 50_000_000, taker: Some(WALLET.into()), slippage_bps: Some(300) }).await.unwrap();
    assert_eq!((order.transaction.as_deref(), order.gasless), (Some("AQID"), true));
    assert!((order.price_impact_pct - 0.42).abs() < 1e-9, "the older fractional field is read as a percentage");
    assert!(server.last().target.contains(&format!("&taker={WALLET}&slippageBps=300&")), "{}", server.last().target);

    // Quoted but not buildable: the venue's reason is passed on.
    server.reply(200, json!({ "outAmount": "1600000", "transaction": "", "errorCode": 2, "errorMessage": "Insufficient SOL for gas", "router": "metis" }));
    let refused = venue.order(&OrderRequest { input_mint: wsol.into(), output_mint: MEME_MINT.into(), amount: 1, taker: Some(WALLET.into()), slippage_bps: None }).await.unwrap_err();
    assert_eq!((refused.code.as_str(), refused.message.as_str()), ("order_2", "Insufficient SOL for gas"));
    // No route, a rate limit, and a quote of nothing.
    server.reply(400, json!({ "error": "No routes found for this pair" }));
    assert!(venue.order(&OrderRequest { input_mint: wsol.into(), output_mint: MEME_MINT.into(), amount: 1, taker: None, slippage_bps: None }).await.unwrap_err().no_route());
    server.reply(429, json!({}));
    assert_eq!(venue.order(&OrderRequest { input_mint: wsol.into(), output_mint: MEME_MINT.into(), amount: 1, taker: None, slippage_bps: None }).await.unwrap_err().code, "rate_limited");
    server.reply(200, json!({ "outAmount": "0", "transaction": null }));
    assert!(venue.order(&OrderRequest { input_mint: wsol.into(), output_mint: MEME_MINT.into(), amount: 1, taker: None, slippage_bps: None }).await.unwrap_err().no_route());

    // Execute: the signed transaction and the request id go back; the fill is read.
    server.reply(200, json!({ "status": "Success", "signature": "sigLanded", "code": 0, "totalInputAmount": "50005000", "inputAmountResult": "50000000", "outputAmountResult": "1600000", "totalOutputAmount": "1590000" }));
    let fill = venue.execute("U0lHTkVE", "r2").await.unwrap();
    assert_eq!((fill.success, fill.signature.as_deref(), fill.input, fill.output), (true, Some("sigLanded"), 50_005_000, 1_590_000));
    let sent = server.last();
    assert_eq!((sent.method.as_str(), sent.target.as_str()), ("POST", "/swap/v2/execute"));
    assert_eq!(serde_json::from_str::<Value>(&sent.body).unwrap(), json!({ "signedTransaction": "U0lHTkVE", "requestId": "r2" }));
    server.reply(200, json!({ "status": "Failed", "signature": "sigExpired", "code": -1005, "error": "Transaction expired" }));
    let failed = venue.execute("U0lHTkVE", "r2").await.unwrap();
    assert_eq!((failed.success, failed.code, failed.error.as_deref()), (false, -1005, Some("Transaction expired")));

    // Nothing listening: a network error, which the engine treats as "outcome unknown".
    let dead = JupiterVenue::new("http://127.0.0.1:9/swap/v2", "", "").unwrap();
    let error = dead.execute("x", "y").await.unwrap_err();
    assert_eq!(error.code, "network");
    assert!(!error.message.contains("127.0.0.1"), "{}", error.message);
}

#[tokio::test]
async fn the_reviewer_client_speaks_the_messages_api() {
    tls_provider();
    let server = MockHttp::start().await;
    let reviewer = AnthropicReviewer::new(&format!("http://{}", server.addr), "sk-test-KEY", "claude-test-model").unwrap();
    assert_eq!(reviewer.model(), "claude-test-model");

    server.reply(200, json!({ "id": "msg_1", "type": "message", "role": "assistant", "content": [{ "type": "text", "text": "{\"verdict\": \"link\", \"confidence\": 0.9, \"reasons\": [\"E4: funded directly\"], \"would_settle\": []}" }], "stop_reason": "end_turn" }));
    let review = reviewer.review(&json!({ "candidate": { "address": WALLET } })).await.unwrap();
    assert_eq!((review.verdict, review.confidence, review.reasons.len()), (ReviewVerdict::Link, 0.9, 1));
    let sent = server.last();
    assert_eq!((sent.method.as_str(), sent.target.as_str()), ("POST", "/v1/messages"));
    assert_eq!(sent.headers.get("x-api-key").map(String::as_str), Some("sk-test-KEY"));
    assert_eq!(sent.headers.get("anthropic-version").map(String::as_str), Some("2023-06-01"));
    assert!(sent.headers.get("content-type").is_some_and(|v| v.starts_with("application/json")));
    let body: Value = serde_json::from_str(&sent.body).unwrap();
    assert_eq!((body["model"].as_str(), body["max_tokens"].as_i64(), body["messages"][0]["role"].as_str()), (Some("claude-test-model"), Some(1024), Some("user")));
    assert!(body["system"].as_str().unwrap().contains("Answer with one JSON object"));
    assert!(body["messages"][0]["content"].as_str().unwrap().contains(WALLET), "the case file is the user message");

    // An API error is reported with its message and never with the key.
    server.reply(401, json!({ "type": "error", "error": { "type": "authentication_error", "message": "invalid x-api-key" } }));
    let error = reviewer.review(&json!({})).await.unwrap_err();
    assert!(error.contains("http 401") && error.contains("invalid x-api-key") && !error.contains("sk-test-KEY"), "{error}");
    // An answer that is not the JSON asked for is an error, not a guess.
    server.reply(200, json!({ "content": [{ "type": "text", "text": "I think they are probably the same person." }] }));
    assert!(reviewer.review(&json!({})).await.is_err());
}

#[tokio::test]
async fn the_key_provider_reads_and_stores_secrets_by_name() {
    tls_provider();
    let server = MockHttp::start().await;
    let base = format!("http://{}", server.addr);
    let keys = DopplerKeys::new("dp.read", "dp.write", "regnerus", "prd").unwrap().with_base(&base);
    assert!(keys.can_store());

    server.reply(200, json!({ "name": "WALLET_ALICE", "value": { "raw": " 5secretBase58 ", "computed": "5secretBase58" } }));
    assert_eq!(keys.reveal("WALLET_ALICE").await.unwrap(), "5secretBase58");
    let asked = server.last();
    assert_eq!((asked.method.as_str(), asked.target.as_str()), ("GET", "/v3/configs/config/secret?project=regnerus&config=prd&name=WALLET_ALICE"));
    assert_eq!(asked.headers.get("authorization").map(String::as_str), Some("Bearer dp.read"));

    server.reply(200, json!({ "secrets": {} }));
    keys.put("REGNERUS_WALLET_7", "5newSecret").await.unwrap();
    let wrote = server.last();
    assert_eq!((wrote.method.as_str(), wrote.target.as_str()), ("POST", "/v3/configs/config/secrets"));
    assert_eq!(wrote.headers.get("authorization").map(String::as_str), Some("Bearer dp.write"), "writes use the write token");
    assert_eq!(serde_json::from_str::<Value>(&wrote.body).unwrap(), json!({ "project": "regnerus", "config": "prd", "secrets": { "REGNERUS_WALLET_7": "5newSecret" } }));

    // Missing, empty, and refused.
    server.reply(404, json!({ "messages": ["not found"] }));
    assert!(keys.reveal("NOPE").await.unwrap_err().contains("returned 404 for secret NOPE"));
    server.reply(200, json!({ "value": { "raw": "" } }));
    assert!(keys.reveal("EMPTY").await.unwrap_err().contains("is empty"));
    server.reply(403, json!({}));
    assert!(keys.put("X", "y").await.unwrap_err().contains("403"));

    // Without a write token nothing can be stored, and nothing is sent.
    let read_only = DopplerKeys::new("dp.read", "", "regnerus", "prd").unwrap().with_base(&base);
    let before = server.seen.lock().len();
    assert!(!read_only.can_store());
    assert!(read_only.put("X", "y").await.unwrap_err().contains("DOPPLER_WRITE_TOKEN"));
    assert_eq!(server.seen.lock().len(), before);
    assert!(DopplerKeys::new("", "", "regnerus", "prd").is_err());
}
