//! Client for the Spotify Web API (`api.spotify.com/v1`), with the account's
//! access token.
//!
//! - 401: the token is renewed once (another source if the same one comes
//!   back), then the request is sent again.
//! - 429: `HttpError` with `Retry-After`, which becomes the protocol's
//!   `rate_limited`.
//! - Answers without a body (`204`, `201` with nothing) are `null`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::Value;

use crate::session::Account;

pub const API_BASE: &str = "https://api.spotify.com/v1";

/// A Web API answer other than 2xx. The body is kept short and never holds
/// a token.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub retry_after: Option<u64>,
    pub body: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.status)?;
        if !self.body.is_empty() {
            write!(f, ": {}", self.body)?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpError {}

impl HttpError {
    /// The status of an API failure somewhere in `e`.
    pub fn status_of(e: &anyhow::Error) -> Option<u16> {
        e.chain().find_map(|c| c.downcast_ref::<HttpError>()).map(|h| h.status)
    }

    /// The endpoint is not served to this client (gone, or cut off for
    /// third-party apps): callers leave the section out.
    pub fn is_withheld(e: &anyhow::Error) -> bool {
        matches!(Self::status_of(e), Some(403 | 404 | 405 | 410 | 501))
    }
}

#[derive(Clone)]
pub struct WebApi {
    base: String,
    http: reqwest::Client,
    account: Arc<Account>,
}

/// `message` of a Web API error body, `{"error": {"status", "message"}}`.
fn error_message(body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    msg.chars().take(200).collect()
}

impl WebApi {
    pub fn new(base: Option<&str>, account: Arc<Account>) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().expect("HTTP client");
        WebApi { base: base.unwrap_or(API_BASE).trim_end_matches('/').to_string(), http, account }
    }

    pub fn account(&self) -> &Arc<Account> {
        &self.account
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.send(Method::GET, path, query, None).await
    }

    pub async fn send(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut rejected: Option<String> = None;
        loop {
            let token = self.account.web_token(rejected.as_deref()).await?;
            let mut req = self.http.request(method.clone(), &url).bearer_auth(&token).query(query);
            if let Some(body) = body {
                req = req.json(body);
            } else if method != Method::GET {
                req = req.header(reqwest::header::CONTENT_LENGTH, 0);
            }
            let resp = req.send().await.with_context(|| format!("{method} {path}"))?;
            let status = resp.status().as_u16();
            if status == 401 && rejected.is_none() {
                tracing::debug!("{method} {path}: token refused, renewing it");
                rejected = Some(token);
                continue;
            }
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let text = resp.text().await.with_context(|| format!("{method} {path}: body"))?;
            if !(200..300).contains(&status) {
                if status == 429 {
                    tracing::warn!("{method} {path}: rate limited, retry after {}s", retry_after.unwrap_or(0));
                }
                let err = HttpError { status, retry_after, body: error_message(&text) };
                return Err(anyhow::Error::from(err).context(format!("{method} {path}")));
            }
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).with_context(|| format!("{method} {path}: not JSON"));
        }
    }

    /// `GET /me`: id, display name, subscription.
    pub async fn me(&self) -> Result<Value> {
        self.get("/me", &[]).await
    }
}

/// `offset` and `limit` as query parameters.
pub fn page(offset: u32, limit: u32) -> [(&'static str, String); 2] {
    [("offset", offset.to_string()), ("limit", limit.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_are_reduced_to_their_message() {
        assert_eq!(error_message(r#"{"error":{"status":404,"message":"Resource not found"}}"#), "Resource not found");
        assert_eq!(error_message("<html>"), "");
        let e = anyhow::Error::from(HttpError { status: 404, retry_after: None, body: String::new() }).context("GET /x");
        assert_eq!(HttpError::status_of(&e), Some(404));
        assert!(HttpError::is_withheld(&e));
        let e = anyhow::Error::from(HttpError { status: 500, retry_after: None, body: String::new() });
        assert!(!HttpError::is_withheld(&e));
    }
}
