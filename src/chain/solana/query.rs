//! Typed readers over the raw JSON-RPC call: the handful of methods the
//! trail engine, the bubble map and the trader need beyond transactions.

use serde_json::{Value, json};

use super::rpc::{ChainRpc, RpcError, SignatureInfo, signature_info};
use crate::obs::budget::Lane;

/// Signatures mentioning the address that are older than `before`, newest first.
pub async fn signatures_before(rpc: &dyn ChainRpc, address: &str, before: Option<&str>, limit: usize, lane: Lane) -> Result<Vec<SignatureInfo>, RpcError> {
    let mut options = json!({ "limit": limit.clamp(1, 1000) });
    if let Some(before) = before {
        options["before"] = json!(before);
    }
    let result = rpc.raw("getSignaturesForAddress", json!([address, options]), lane).await?;
    Ok(result.as_array().map(|rows| rows.iter().filter_map(signature_info).collect()).unwrap_or_default())
}

pub async fn balance(rpc: &dyn ChainRpc, address: &str, lane: Lane) -> Result<u64, RpcError> {
    let result = rpc.raw("getBalance", json!([address]), lane).await?;
    Ok(result.get("value").and_then(Value::as_u64).or_else(|| result.as_u64()).unwrap_or(0))
}

/// What a wallet holds of one mint, across its token accounts, in raw units.
pub async fn token_balance(rpc: &dyn ChainRpc, owner: &str, mint: &str, lane: Lane) -> Result<u64, RpcError> {
    let result = rpc.raw("getTokenAccountsByOwner", json!([owner, { "mint": mint }, { "encoding": "jsonParsed" }]), lane).await?;
    let accounts = result.get("value").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(accounts.iter().filter_map(|a| a.pointer("/account/data/parsed/info/tokenAmount/amount")?.as_str()?.parse::<u64>().ok()).sum())
}

/// A token's largest token accounts: `(token account, raw amount)`, largest first.
pub async fn largest_accounts(rpc: &dyn ChainRpc, mint: &str, lane: Lane) -> Result<Vec<(String, u64)>, RpcError> {
    let result = rpc.raw("getTokenLargestAccounts", json!([mint]), lane).await?;
    let rows = result.get("value").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(rows.iter().filter_map(|row| Some((row.get("address")?.as_str()?.to_string(), row.get("amount")?.as_str()?.parse().ok()?))).collect())
}

pub async fn token_supply(rpc: &dyn ChainRpc, mint: &str, lane: Lane) -> Result<u64, RpcError> {
    let result = rpc.raw("getTokenSupply", json!([mint]), lane).await?;
    Ok(result.pointer("/value/amount").and_then(Value::as_str).and_then(|a| a.parse().ok()).unwrap_or(0))
}

/// Accounts in `jsonParsed` form, in the order asked; `Null` for one that does not exist.
pub async fn accounts(rpc: &dyn ChainRpc, addresses: &[String], lane: Lane) -> Result<Vec<Value>, RpcError> {
    let mut out = Vec::with_capacity(addresses.len());
    for chunk in addresses.chunks(100) {
        let result = rpc.raw("getMultipleAccounts", json!([chunk, { "encoding": "jsonParsed" }]), lane).await?;
        let mut rows = result.get("value").and_then(Value::as_array).cloned().unwrap_or_default();
        rows.resize(chunk.len(), Value::Null);
        out.extend(rows);
    }
    Ok(out)
}

/// What a mint account says about who controls the token.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MintFacts {
    pub exists: bool,
    pub decimals: u8,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    /// Token-2022 extension names, as the node spells them.
    pub extensions: Vec<String>,
    pub token_2022: bool,
}

pub fn mint_facts(account: &Value) -> MintFacts {
    let Some(info) = account.pointer("/data/parsed/info") else { return MintFacts::default() };
    let text = |key: &str| info.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    MintFacts {
        exists: true,
        decimals: info.get("decimals").and_then(Value::as_u64).unwrap_or(0) as u8,
        mint_authority: text("mintAuthority"),
        freeze_authority: text("freezeAuthority"),
        extensions: info.get("extensions").and_then(Value::as_array).map(|list| list.iter().filter_map(|e| e.get("extension")?.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        token_2022: account.pointer("/data/program").and_then(Value::as_str) == Some("spl-token-2022"),
    }
}
