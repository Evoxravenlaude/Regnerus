//! Key custody. A trading wallet's key is held by a secrets provider and
//! named by a reference; the database stores the reference and never the
//! key. Resolved here, at signing time only.
//!
//!   doppler: the reference is a secret name in the configured project (recommended)
//!   env:     the reference is an environment variable holding the key

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

#[async_trait]
pub trait KeyProvider: Send + Sync {
    /// The secret behind a reference. Never logged.
    async fn reveal(&self, reference: &str) -> Result<String, String>;
    /// Stores a new secret. Providers that cannot keep one safely refuse.
    async fn put(&self, reference: &str, value: &str) -> Result<(), String>;
    fn can_store(&self) -> bool;
    fn name(&self) -> &'static str;
}

/// A reference is a secret name: capitals, digits and underscores.
pub fn valid_reference(reference: &str) -> bool {
    !reference.is_empty() && reference.len() <= 64 && reference.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && !reference.starts_with(|c: char| c.is_ascii_digit())
}

pub struct EnvKeys;

#[async_trait]
impl KeyProvider for EnvKeys {
    async fn reveal(&self, reference: &str) -> Result<String, String> {
        std::env::var(reference).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).ok_or_else(|| format!("environment variable {reference} is not set"))
    }

    async fn put(&self, _reference: &str, _value: &str) -> Result<(), String> {
        // A key kept only in this process would be lost at the next restart, with the funds.
        Err("the env provider cannot store a new key; set SECRETS_PROVIDER=doppler, or create the key yourself and point /usewallet at its variable".to_string())
    }

    fn can_store(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "env"
    }
}

pub struct DopplerKeys {
    token: String,
    write_token: String,
    project: String,
    config: String,
    client: reqwest::Client,
    base: String,
}

impl DopplerKeys {
    pub fn new(token: &str, write_token: &str, project: &str, config: &str) -> Result<Self, String> {
        if token.is_empty() || project.is_empty() || config.is_empty() {
            return Err("SECRETS_PROVIDER=doppler needs DOPPLER_TOKEN, DOPPLER_PROJECT and DOPPLER_CONFIG".to_string());
        }
        let client = reqwest::Client::builder().timeout(Duration::from_secs(8)).build().map_err(|e| e.to_string())?;
        Ok(DopplerKeys {
            token: token.to_string(),
            write_token: write_token.to_string(),
            project: project.to_string(),
            config: config.to_string(),
            client,
            base: "https://api.doppler.com".to_string(),
        })
    }

    /// Points the client at another host. Tests only.
    pub fn with_base(mut self, base: &str) -> Self {
        self.base = base.trim_end_matches('/').to_string();
        self
    }
}

#[async_trait]
impl KeyProvider for DopplerKeys {
    async fn reveal(&self, reference: &str) -> Result<String, String> {
        let response = self
            .client
            .get(format!("{}/v3/configs/config/secret", self.base))
            .query(&[("project", self.project.as_str()), ("config", self.config.as_str()), ("name", reference)])
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("doppler could not be reached: {}", e.without_url()))?;
        if !response.status().is_success() {
            return Err(format!("doppler returned {} for secret {reference}", response.status().as_u16()));
        }
        let body: Value = response.json().await.map_err(|_| "doppler sent an unreadable answer".to_string())?;
        body.pointer("/value/raw").and_then(Value::as_str).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).ok_or_else(|| format!("doppler secret {reference} is empty"))
    }

    async fn put(&self, reference: &str, value: &str) -> Result<(), String> {
        if self.write_token.is_empty() {
            return Err("DOPPLER_WRITE_TOKEN is not set, so new keys cannot be stored".to_string());
        }
        let response = self
            .client
            .post(format!("{}/v3/configs/config/secrets", self.base))
            .bearer_auth(&self.write_token)
            .json(&json!({ "project": self.project, "config": self.config, "secrets": { reference: value } }))
            .send()
            .await
            .map_err(|e| format!("doppler could not be reached: {}", e.without_url()))?;
        if response.status().is_success() { Ok(()) } else { Err(format!("doppler write returned {}", response.status().as_u16())) }
    }

    fn can_store(&self) -> bool {
        !self.write_token.is_empty()
    }

    fn name(&self) -> &'static str {
        "doppler"
    }
}
