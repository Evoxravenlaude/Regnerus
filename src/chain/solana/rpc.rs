//! Solana JSON-RPC: the two calls the watcher needs, paced by the budget.
//!
//! The URL usually carries an API key, so it is never logged; only its host is.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::obs::budget::{Budget, Lane};
use crate::util::host_of;

/// JSON-RPC's own code for "too many requests", as Solana nodes send it.
const RPC_RATE_LIMITED: i64 = -32429;
const ATTEMPTS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commitment {
    Processed,
    Confirmed,
    Finalized,
}

impl Commitment {
    pub fn parse(text: &str) -> Option<Commitment> {
        match text.trim().to_ascii_lowercase().as_str() {
            "processed" => Some(Commitment::Processed),
            "confirmed" => Some(Commitment::Confirmed),
            "finalized" => Some(Commitment::Finalized),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Commitment::Processed => "processed",
            Commitment::Confirmed => "confirmed",
            Commitment::Finalized => "finalized",
        }
    }

    /// `getTransaction` and `getSignaturesForAddress` do not accept
    /// `processed`; the nearest level they do accept is `confirmed`.
    pub fn for_history(&self) -> Commitment {
        match self {
            Commitment::Processed => Commitment::Confirmed,
            other => *other,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RpcError {
    RateLimited,
    Http(u16),
    Rpc {
        code: i64,
        message: String,
    },
    Transport(String),
    Decode(String),
    /// The budget refused the request (forensics share spent).
    Budget(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::RateLimited => write!(f, "the RPC is rate limiting"),
            RpcError::Http(status) => write!(f, "the RPC answered HTTP {status}"),
            RpcError::Rpc { code, message } => write!(f, "RPC error {code}: {message}"),
            RpcError::Transport(message) => write!(f, "could not reach the RPC: {message}"),
            RpcError::Decode(message) => write!(f, "unreadable RPC answer: {message}"),
            RpcError::Budget(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for RpcError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureInfo {
    pub signature: String,
    pub slot: u64,
    pub failed: bool,
    pub block_time: Option<i64>,
}

/// What the watcher asks of a node. Tests answer from memory.
#[async_trait]
pub trait ChainRpc: Send + Sync {
    /// The `jsonParsed` transaction, or `None` when the node does not have it (yet).
    async fn get_transaction(&self, signature: &str, lane: Lane) -> Result<Option<Value>, RpcError>;
    /// Signatures mentioning the address, newest first, stopping before `until`.
    async fn get_signatures(&self, address: &str, until: Option<&str>, limit: usize, lane: Lane) -> Result<Vec<SignatureInfo>, RpcError>;
    /// Any other JSON-RPC method, answered with its bare `result`. The
    /// typed readers in `query` are built on this.
    async fn raw(&self, method: &str, params: Value, lane: Lane) -> Result<Value, RpcError>;
}

pub struct SolanaRpc {
    url: String,
    host: String,
    client: reqwest::Client,
    budget: Arc<Budget>,
    commitment: Commitment,
    next_id: AtomicU64,
}

impl SolanaRpc {
    pub fn new(url: &str, commitment: Commitment, budget: Arc<Budget>) -> Result<Self, String> {
        let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(20)).pool_max_idle_per_host(8).build().map_err(|e| e.to_string())?;
        Ok(SolanaRpc { url: url.to_string(), host: host_of(url), client, budget, commitment: commitment.for_history(), next_id: AtomicU64::new(1) })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// A reqwest error can quote the URL, key included.
    fn redact(&self, text: String) -> String {
        text.replace(&self.url, &self.host)
    }

    async fn post_once(&self, body: &Value) -> Result<Value, RpcError> {
        let response = self.client.post(&self.url).json(body).send().await.map_err(|e| RpcError::Transport(self.redact(e.to_string())))?;
        let status = response.status().as_u16();
        if status == 429 {
            let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(0.0);
            self.budget.on_rate_limited(retry_after);
            return Err(RpcError::RateLimited);
        }
        if !(200..300).contains(&status) {
            return Err(RpcError::Http(status));
        }
        let root: Value = response.json().await.map_err(|e| RpcError::Decode(self.redact(e.to_string())))?;
        if let Some(error) = root.get("error").filter(|e| !e.is_null()) {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error.get("message").and_then(Value::as_str).unwrap_or("").chars().take(200).collect::<String>();
            if code == RPC_RATE_LIMITED {
                self.budget.on_rate_limited(0.0);
                return Err(RpcError::RateLimited);
            }
            return Err(RpcError::Rpc { code, message });
        }
        self.budget.on_ok();
        Ok(root.get("result").cloned().unwrap_or(Value::Null))
    }

    /// One call, retried on rate limits, 5xx answers and transport errors.
    pub async fn call(&self, method: &str, params: Value, lane: Lane) -> Result<Value, RpcError> {
        let mut last = RpcError::Transport("no attempt was made".into());
        for attempt in 0..ATTEMPTS {
            self.budget.acquire(lane, 1).await.map_err(|e| RpcError::Budget(e.to_string()))?;
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            match self.post_once(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await {
                Ok(result) => return Ok(result),
                // The budget is now standing down; the next acquire waits it out.
                Err(RpcError::RateLimited) => last = RpcError::RateLimited,
                Err(error @ (RpcError::Transport(_) | RpcError::Decode(_))) => last = error,
                Err(RpcError::Http(status)) if status >= 500 => last = RpcError::Http(status),
                Err(other) => return Err(other),
            }
            if attempt + 1 < ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
            }
        }
        log::warn!("{method} on {} failed after {ATTEMPTS} attempts: {last}", self.host);
        Err(last)
    }
}

/// Reads one row of `getSignaturesForAddress`.
pub fn signature_info(row: &Value) -> Option<SignatureInfo> {
    Some(SignatureInfo {
        signature: row.get("signature")?.as_str()?.to_string(),
        slot: row.get("slot").and_then(Value::as_u64).unwrap_or(0),
        failed: row.get("err").is_some_and(|e| !e.is_null()),
        block_time: row.get("blockTime").and_then(Value::as_i64),
    })
}

#[async_trait]
impl ChainRpc for SolanaRpc {
    async fn get_transaction(&self, signature: &str, lane: Lane) -> Result<Option<Value>, RpcError> {
        let params = json!([signature, { "encoding": "jsonParsed", "commitment": self.commitment.as_str(), "maxSupportedTransactionVersion": 0 }]);
        let result = self.call("getTransaction", params, lane).await?;
        Ok(if result.is_null() { None } else { Some(result) })
    }

    async fn get_signatures(&self, address: &str, until: Option<&str>, limit: usize, lane: Lane) -> Result<Vec<SignatureInfo>, RpcError> {
        let mut options = json!({ "limit": limit.clamp(1, 1000), "commitment": self.commitment.as_str() });
        if let Some(until) = until {
            options["until"] = json!(until);
        }
        let result = self.call("getSignaturesForAddress", json!([address, options]), lane).await?;
        let rows = result.as_array().ok_or_else(|| RpcError::Decode("getSignaturesForAddress did not return a list".into()))?;
        Ok(rows.iter().filter_map(signature_info).collect())
    }

    async fn raw(&self, method: &str, params: Value, lane: Lane) -> Result<Value, RpcError> {
        self.call(method, params, lane).await
    }
}
