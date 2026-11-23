//! Where swaps are quoted and sent. Jupiter's Swap API V2 behind a trait,
//! so the engine is tested against a fake.
//!
//!   GET  {base}/order?inputMint&outputMint&amount[&taker][&slippageBps][&excludeRouters]
//!        Without `taker` the answer is a quote; with it, a transaction to sign.
//!   POST {base}/execute { signedTransaction, requestId }
//!        Jupiter lands the transaction and reports what it did.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::util::host_of;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenueError {
    /// `no_route`, `rate_limited`, `network`, `http_500`, or the venue's own code.
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for VenueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl VenueError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        VenueError { code: code.into(), message: message.into() }
    }

    /// The token cannot be sold or bought at all right now, as opposed to the venue being unreachable.
    pub fn no_route(&self) -> bool {
        self.code == "no_route"
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderRequest {
    pub input_mint: String,
    pub output_mint: String,
    pub amount: u64,
    /// The signing wallet. `None` asks for a quote only.
    pub taker: Option<String>,
    pub slippage_bps: Option<u16>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Order {
    pub in_amount: u64,
    pub out_amount: u64,
    /// In percentage points; negative when the trade moves the price against us.
    pub price_impact_pct: f64,
    /// Base64, unsigned. `None` for a quote.
    pub transaction: Option<String>,
    pub request_id: String,
    /// Someone else pays the fee: the transaction then has a second signer.
    pub gasless: bool,
    pub router: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    pub success: bool,
    pub signature: Option<String>,
    pub code: i64,
    pub error: Option<String>,
    /// What left the wallet, fees included.
    pub input: u64,
    /// What reached the wallet, after fees.
    pub output: u64,
}

#[async_trait]
pub trait SwapVenue: Send + Sync {
    async fn order(&self, request: &OrderRequest) -> Result<Order, VenueError>;
    async fn execute(&self, signed_transaction: &str, request_id: &str) -> Result<Fill, VenueError>;
    fn name(&self) -> String;
}

pub struct JupiterVenue {
    base: String,
    api_key: String,
    exclude_routers: String,
    client: reqwest::Client,
}

fn amount_of(value: &Value, key: &str) -> u64 {
    match value.get(key) {
        Some(Value::String(text)) => text.parse().unwrap_or(0),
        Some(other) => other.as_u64().unwrap_or(0),
        None => 0,
    }
}

impl JupiterVenue {
    pub fn new(base: &str, api_key: &str, exclude_routers: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
        Ok(JupiterVenue { base: base.trim_end_matches('/').to_string(), api_key: api_key.to_string(), exclude_routers: exclude_routers.to_string(), client })
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, VenueError> {
        let request = if self.api_key.is_empty() { request } else { request.header("x-api-key", &self.api_key) };
        let response = request.send().await.map_err(|e| VenueError::new("network", e.without_url().to_string()))?;
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if status == 429 {
            return Err(VenueError::new("rate_limited", "the swap venue is rate-limiting this bot"));
        }
        if !(200..300).contains(&status) {
            let message = body.get("error").or_else(|| body.get("errorMessage")).and_then(Value::as_str).unwrap_or("the swap venue refused the request").chars().take(200).collect::<String>();
            let lowered = message.to_ascii_lowercase();
            let code = if lowered.contains("route") || lowered.contains("not tradable") || lowered.contains("liquidity") { "no_route".to_string() } else { format!("http_{status}") };
            return Err(VenueError::new(code, message));
        }
        Ok(body)
    }
}

#[async_trait]
impl SwapVenue for JupiterVenue {
    async fn order(&self, request: &OrderRequest) -> Result<Order, VenueError> {
        let mut query: Vec<(&str, String)> = vec![("inputMint", request.input_mint.clone()), ("outputMint", request.output_mint.clone()), ("amount", request.amount.to_string())];
        if let Some(taker) = &request.taker {
            query.push(("taker", taker.clone()));
        }
        if let Some(bps) = request.slippage_bps {
            query.push(("slippageBps", bps.to_string()));
        }
        if !self.exclude_routers.is_empty() {
            query.push(("excludeRouters", self.exclude_routers.clone()));
        }
        let body = self.send(self.client.get(format!("{}/order", self.base)).query(&query)).await?;
        let out_amount = amount_of(&body, "outAmount");
        let transaction = body.get("transaction").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string);
        if request.taker.is_some() && transaction.is_none() {
            // Quoted but not buildable: the venue says why with a code and a message.
            let message = body.get("errorMessage").or_else(|| body.get("error")).and_then(Value::as_str).unwrap_or("the venue returned no transaction");
            return Err(VenueError::new(format!("order_{}", body.get("errorCode").and_then(Value::as_i64).unwrap_or(0)), message.chars().take(200).collect::<String>()));
        }
        if out_amount == 0 {
            return Err(VenueError::new("no_route", "no route for this token right now"));
        }
        Ok(Order {
            in_amount: amount_of(&body, "inAmount"),
            out_amount,
            price_impact_pct: body
                .get("priceImpact")
                .and_then(Value::as_f64)
                .or_else(|| body.get("priceImpactPct").and_then(Value::as_str).and_then(|p| p.parse::<f64>().ok()).map(|p| p * 100.0))
                .unwrap_or(0.0),
            transaction,
            request_id: body.get("requestId").and_then(Value::as_str).unwrap_or("").to_string(),
            gasless: body.get("gasless").and_then(Value::as_bool).unwrap_or(false),
            router: body.get("router").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }

    async fn execute(&self, signed_transaction: &str, request_id: &str) -> Result<Fill, VenueError> {
        let body = self.send(self.client.post(format!("{}/execute", self.base)).json(&json!({ "signedTransaction": signed_transaction, "requestId": request_id }))).await?;
        Ok(Fill {
            success: body.get("status").and_then(Value::as_str) == Some("Success"),
            signature: body.get("signature").and_then(Value::as_str).map(str::to_string),
            code: body.get("code").and_then(Value::as_i64).unwrap_or(0),
            error: body.get("error").and_then(Value::as_str).map(|e| e.chars().take(200).collect()),
            input: amount_of(&body, "totalInputAmount").max(amount_of(&body, "inputAmountResult")),
            output: match amount_of(&body, "totalOutputAmount") {
                0 => amount_of(&body, "outputAmountResult"),
                total => total,
            },
        })
    }

    fn name(&self) -> String {
        host_of(&self.base)
    }
}
