//! Small helpers shared by every module: clocks, address checks, number and
//! HTML formatting.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Wall clock in unix seconds. Injected so tests can move time.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
}

/// UTC day as `YYYY-MM-DD`, the key of the credit ledger.
pub fn day_of(unix_secs: i64) -> String {
    chrono::DateTime::from_timestamp(unix_secs, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "1970-01-01".to_string())
}

/// A Solana address is 32 bytes of base58.
pub fn is_solana_address(text: &str) -> bool {
    (32..=44).contains(&text.len()) && text.is_ascii() && bs58::decode(text).into_vec().map(|bytes| bytes.len() == 32).unwrap_or(false)
}

/// A transaction signature is 64 bytes of base58.
pub fn is_signature(text: &str) -> bool {
    (64..=90).contains(&text.len()) && text.is_ascii() && bs58::decode(text).into_vec().map(|bytes| bytes.len() == 64).unwrap_or(false)
}

/// `7xKp…3fQa`. Anything too short to shorten is returned whole.
pub fn short(address: &str) -> String {
    let chars: Vec<char> = address.chars().collect();
    if chars.len() <= 10 {
        return address.to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Telegram HTML: only these three characters need escaping.
pub fn esc(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Base units to a decimal string with at most `places` fraction digits,
/// trailing zeros removed: `fmt_units(101_000_000, 9, 4) == "0.101"`.
pub fn fmt_units(raw: u64, decimals: u8, places: usize) -> String {
    let scale = 10u128.pow(decimals as u32);
    let raw = raw as u128;
    let whole = raw / scale;
    let frac = raw % scale;
    if decimals == 0 || places == 0 {
        return group_thousands(whole);
    }
    let mut frac_text = format!("{:0width$}", frac, width = decimals as usize);
    frac_text.truncate(places.min(decimals as usize));
    let frac_text = frac_text.trim_end_matches('0');
    if frac_text.is_empty() { group_thousands(whole) } else { format!("{}.{}", group_thousands(whole), frac_text) }
}

/// Token amounts for cards: `1.23K`, `4.5M`, `3.5B`; small amounts in full.
pub fn fmt_compact(raw: u64, decimals: u8) -> String {
    let value = raw as f64 / 10f64.powi(decimals as i32);
    let (scaled, suffix) = if value >= 1e12 {
        (value / 1e12, "T")
    } else if value >= 1e9 {
        (value / 1e9, "B")
    } else if value >= 1e6 {
        (value / 1e6, "M")
    } else if value >= 1e3 {
        (value / 1e3, "K")
    } else {
        (value, "")
    };
    let text = if scaled >= 100.0 {
        format!("{scaled:.0}")
    } else if scaled >= 10.0 {
        format!("{scaled:.1}")
    } else {
        format!("{scaled:.2}")
    };
    let text = if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.').to_string() } else { text };
    format!("{text}{suffix}")
}

fn group_thousands(value: u128) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `1 wallet`, `2 wallets`: a number with the right form of its noun.
pub fn count(n: impl std::fmt::Display, one: &str, many: &str) -> String {
    let n = n.to_string();
    let noun = if n == "1" { one } else { many };
    format!("{n} {noun}")
}

/// `45s`, `12m`, `3h`, `2d`: how long ago something happened.
pub fn ago(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 90 {
        format!("{s}s")
    } else if s < 90 * 60 {
        format!("{}m", s / 60)
    } else if s < 36 * 3600 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// The host of a URL, for logs. RPC URLs carry API keys in the path or
/// query, so the full URL must never be logged.
pub fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    host.to_string()
}
