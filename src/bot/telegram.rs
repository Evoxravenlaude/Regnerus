//! The Telegram Bot API, spoken directly over HTTPS as the Kizzy watcher
//! does. Everything goes through one trait so tests answer from memory.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

const API: &str = "https://api.telegram.org";
pub const LONG_POLL_S: u64 = 25;

#[derive(Clone, Debug, PartialEq)]
pub struct TgReply {
    pub ok: bool,
    pub result: Value,
    pub error: String,
}

impl TgReply {
    pub fn ok(result: Value) -> Self {
        TgReply { ok: true, result, error: String::new() }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        TgReply { ok: false, result: Value::Null, error: error.into() }
    }
}

/// One Bot API call; tests answer from memory.
#[async_trait]
pub trait TelegramCall: Send + Sync {
    async fn call(&self, method: &str, payload: Value, long_poll: bool) -> TgReply;
}

/// The real Bot API, with flood-wait retries and dry runs.
pub struct TelegramHttp {
    token: String,
    dry_run: bool,
    client: reqwest::Client,
    fake_id: AtomicI64,
}

impl TelegramHttp {
    pub fn new(token: &str, dry_run: bool) -> Result<Self, String> {
        let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).pool_max_idle_per_host(4).build().map_err(|e| e.to_string())?;
        Ok(TelegramHttp { token: token.to_string(), dry_run, client, fake_id: AtomicI64::new(0) })
    }

    /// The token is in every URL; it must never reach a log line or a chat.
    fn redact(&self, text: String) -> String {
        if self.token.is_empty() { text } else { text.replace(&self.token, "<token>") }
    }

    async fn post_once(&self, method: &str, payload: &Value, timeout: Duration) -> Result<Value, String> {
        let url = format!("{API}/bot{}/{method}", self.token);
        let response = self.client.post(url).json(payload).timeout(timeout).send().await.map_err(|e| e.to_string())?;
        response.json::<Value>().await.map_err(|e| e.to_string())
    }
}

#[async_trait]
impl TelegramCall for TelegramHttp {
    async fn call(&self, method: &str, payload: Value, long_poll: bool) -> TgReply {
        if self.dry_run {
            let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
            return match method {
                "sendMessage" => {
                    let id = self.fake_id.fetch_add(1, Ordering::Relaxed) + 1;
                    log::info!("[dry-run] send #{id} to {}:\n{text}", payload.get("chat_id").cloned().unwrap_or(Value::Null));
                    TgReply::ok(json!({ "message_id": id }))
                }
                "editMessageText" => {
                    log::info!("[dry-run] edit #{}:\n{text}", payload.get("message_id").cloned().unwrap_or(Value::Null));
                    TgReply::ok(json!({}))
                }
                "getUpdates" => {
                    // Nobody is on the other end of a dry run.
                    tokio::time::sleep(Duration::from_secs(LONG_POLL_S)).await;
                    TgReply::ok(json!([]))
                }
                _ => TgReply::ok(json!({})),
            };
        }

        let timeout = Duration::from_secs(if long_poll { LONG_POLL_S + 15 } else { 15 });
        for attempt in 0..3u64 {
            let root = match self.post_once(method, &payload, timeout).await {
                Ok(root) => root,
                Err(error) => {
                    if attempt == 2 {
                        return TgReply::failed(self.redact(error));
                    }
                    tokio::time::sleep(Duration::from_secs(1 + attempt)).await;
                    continue;
                }
            };
            if root.get("ok").and_then(Value::as_bool) == Some(true) {
                return TgReply::ok(root.get("result").cloned().unwrap_or(Value::Null));
            }
            let description: String = root.get("description").and_then(Value::as_str).unwrap_or("").chars().take(200).collect();
            let retry_after = root.get("parameters").and_then(|p| p.get("retry_after")).and_then(Value::as_f64);
            if root.get("error_code").and_then(Value::as_i64) == Some(429)
                && attempt < 2
                && let Some(wait) = retry_after.filter(|w| *w > 0.0)
            {
                log::warn!("telegram flood limit on {method}; waiting {wait}s");
                tokio::time::sleep(Duration::from_secs_f64(wait + 0.5)).await;
                continue;
            }
            if !description.contains("message is not modified") {
                log::warn!("telegram {method} refused: {description}");
            }
            return TgReply::failed(description);
        }
        TgReply::failed("no attempt was made")
    }
}

/// An inline button under a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Button {
    /// Sends `data` back to the bot as a callback query.
    Callback {
        text: String,
        data: String,
    },
    Url {
        text: String,
        url: String,
    },
}

impl Button {
    pub fn callback(text: impl Into<String>, data: impl Into<String>) -> Button {
        Button::Callback { text: text.into(), data: data.into() }
    }

    pub fn url(text: impl Into<String>, url: impl Into<String>) -> Button {
        Button::Url { text: text.into(), url: url.into() }
    }

    fn json(&self) -> Value {
        match self {
            Button::Callback { text, data } => json!({ "text": text, "callback_data": data }),
            Button::Url { text, url } => json!({ "text": text, "url": url }),
        }
    }
}

pub type Keyboard = Vec<Vec<Button>>;

fn markup(keyboard: &Keyboard) -> Value {
    json!({ "inline_keyboard": keyboard.iter().map(|row| row.iter().map(Button::json).collect::<Vec<_>>()).collect::<Vec<_>>() })
}

/// What one message needs: HTML text and, optionally, buttons.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Card {
    pub text: String,
    pub keyboard: Keyboard,
}

impl Card {
    pub fn text(text: impl Into<String>) -> Card {
        Card { text: text.into(), keyboard: Vec::new() }
    }
}

/// Telegram refuses a message over 4096 characters. A longer card is cut
/// at a line end, where its HTML tags are closed, and says so.
pub fn fit(text: &str) -> String {
    const LIMIT: usize = 3_900;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for line in text.lines() {
        let length = line.chars().count() + 1;
        if used + length > LIMIT {
            break;
        }
        out.push_str(line);
        out.push('\n');
        used += length;
    }
    out.push_str("… (cut short: too long for one message)");
    out
}

fn message_payload(chat: i64, card: &Card) -> Value {
    let mut payload = json!({ "chat_id": chat, "text": fit(&card.text), "parse_mode": "HTML", "link_preview_options": { "is_disabled": true } });
    if !card.keyboard.is_empty() {
        payload["reply_markup"] = markup(&card.keyboard);
    }
    payload
}

/// Sends a card. Returns the new message's id.
pub async fn send(tg: &dyn TelegramCall, chat: i64, card: &Card) -> Option<i64> {
    let reply = tg.call("sendMessage", message_payload(chat, card), false).await;
    if !reply.ok {
        return None;
    }
    reply.result.get("message_id").and_then(Value::as_i64)
}

/// Replaces a message's text and buttons in place.
pub async fn edit(tg: &dyn TelegramCall, chat: i64, message_id: i64, card: &Card) -> bool {
    let mut payload = message_payload(chat, card);
    payload["message_id"] = json!(message_id);
    if card.keyboard.is_empty() {
        payload["reply_markup"] = json!({ "inline_keyboard": [] });
    }
    let reply = tg.call("editMessageText", payload, false).await;
    reply.ok || reply.error.contains("message is not modified")
}

/// Stops the spinner on a pressed button, with an optional toast.
pub async fn answer_callback(tg: &dyn TelegramCall, callback_id: &str, text: &str) {
    let mut payload = json!({ "callback_query_id": callback_id });
    if !text.is_empty() {
        payload["text"] = json!(text);
    }
    tg.call("answerCallbackQuery", payload, false).await;
}
