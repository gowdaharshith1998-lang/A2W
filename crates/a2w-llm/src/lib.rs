//! # a2w-llm
//!
//! A tiny, **provider-agnostic** LLM client used only at *authoring time* (to
//! turn a plain-English prompt into a workflow IR). It is deliberately minimal:
//! a single [`LlmClient`] trait with one `complete(system, user)` method, an
//! [`AnthropicClient`] implementation over the Anthropic Messages API, and a
//! deterministic [`MockLlm`] for network-free tests.
//!
//! Nothing here runs at *workflow execution* time — workflows never call an LLM
//! to run; this crate is purely the bridge from natural language to IR.
//!
//! ## Why a trait
//! The author loop ([`a2w_author`](https://docs.rs/a2w-author)) depends on
//! [`LlmClient`], not on Anthropic specifically. Tests inject [`MockLlm`] to get
//! fully deterministic Generate→Validate→Repair behaviour with **no network**.

#![forbid(unsafe_code)]

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;
use thiserror::Error;

/// Errors an [`LlmClient`] can surface.
#[derive(Debug, Error)]
pub enum LlmError {
    /// Misconfiguration (e.g. a required environment variable is missing).
    #[error("llm configuration error: {0}")]
    Config(String),
    /// A transport-level failure talking to the provider (DNS, TLS, timeout,
    /// body decode).
    #[error("llm http error: {0}")]
    Http(String),
    /// The provider returned a non-success status, or a success body that did
    /// not match the expected shape.
    #[error("llm api error: {0}")]
    Api(String),
}

/// A provider-agnostic single-turn completion interface.
///
/// One call maps a `system` prompt plus a `user` message to assistant text.
/// Implementations must be `Send + Sync` so the author loop can hold one behind
/// a shared reference across `await` points.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Produce assistant text for the given `system` + `user` prompts.
    ///
    /// # Errors
    /// Returns [`LlmError`] on configuration, transport, or API-shape failures.
    async fn complete(&self, system: &str, user: &str) -> Result<String, LlmError>;
}

/// Default model id used when `A2W_LLM_MODEL` is not set.
const DEFAULT_MODEL: &str = "claude-opus-4-8";
/// Default API base URL used when `A2W_LLM_BASE_URL` is not set.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Anthropic API version header value this client speaks.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Token budget for a single authoring completion.
const MAX_TOKENS: u32 = 4096;

/// An [`LlmClient`] backed by the Anthropic Messages API.
///
/// Construct via [`AnthropicClient::from_env`] (reads `ANTHROPIC_API_KEY` and
/// optional `A2W_LLM_MODEL` / `A2W_LLM_BASE_URL`) or [`AnthropicClient::new`].
#[derive(Debug, Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl AnthropicClient {
    /// Build a client from environment variables.
    ///
    /// - `ANTHROPIC_API_KEY` — **required**; missing/empty yields
    ///   [`LlmError::Config`] with an actionable message.
    /// - `A2W_LLM_MODEL` — optional; defaults to `"claude-opus-4-8"`.
    /// - `A2W_LLM_BASE_URL` — optional; defaults to `"https://api.anthropic.com"`.
    ///
    /// # Errors
    /// [`LlmError::Config`] if `ANTHROPIC_API_KEY` is unset or empty.
    pub fn from_env() -> Result<Self, LlmError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                LlmError::Config(
                    "ANTHROPIC_API_KEY is not set; set it to your Anthropic API key. \
                     A2W_LLM_MODEL (default claude-opus-4-8) and A2W_LLM_BASE_URL \
                     (default https://api.anthropic.com) are optional."
                        .to_string(),
                )
            })?;

        let model = std::env::var("A2W_LLM_MODEL")
            .ok()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let base_url = std::env::var("A2W_LLM_BASE_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        Ok(Self {
            http: reqwest::Client::new(),
            api_key,
            model,
            base_url,
        })
    }

    /// Build a client with an explicit key and model, using the default base URL
    /// and a fresh HTTP client. Does not touch the environment or the network.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// The configured model id.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The configured base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, LlmError> {
        // Trim any trailing slash so we don't emit `//v1/messages`.
        let endpoint = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

        let body = json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": [ { "role": "user", "content": user } ],
        });

        let resp = self
            .http
            .post(&endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        if !status.is_success() {
            return Err(LlmError::Api(format!("HTTP {status}: {text}")));
        }

        // Parse `{ "content": [ { "type": "text", "text": "..." }, ... ] }` and
        // concatenate the text of every `text` block.
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Api(format!("response was not valid JSON ({e}): {text}")))?;

        let blocks = parsed
            .get("content")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| LlmError::Api(format!("response missing a `content` array: {text}")))?;

        let mut out = String::new();
        for block in blocks {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                if let Some(t) = block.get("text").and_then(serde_json::Value::as_str) {
                    out.push_str(t);
                }
            }
        }

        if out.is_empty() {
            return Err(LlmError::Api(format!(
                "response contained no text content blocks: {text}"
            )));
        }

        Ok(out)
    }
}

/// A deterministic, network-free [`LlmClient`] for tests.
///
/// Constructed from a queue of canned responses. Each [`complete`](LlmClient::complete)
/// call returns the next response; once the queue is exhausted it keeps
/// returning the **last** response. A single-response mock therefore always
/// returns that one response no matter how many times it is called.
///
/// Internally a [`Mutex`] guards a cursor so the type is `Send + Sync` and can be
/// shared across `await` points.
#[derive(Debug)]
pub struct MockLlm {
    responses: Vec<String>,
    cursor: Mutex<usize>,
}

impl MockLlm {
    /// Build a mock from a list of responses (returned in order, last repeats).
    ///
    /// An empty list is permitted and yields an empty string from every call.
    #[must_use]
    pub fn new(responses: Vec<String>) -> Self {
        Self {
            responses,
            cursor: Mutex::new(0),
        }
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String, LlmError> {
        if self.responses.is_empty() {
            return Ok(String::new());
        }
        // Recover from a poisoned lock rather than panicking on untrusted use.
        let mut cursor = match self.cursor.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let idx = (*cursor).min(self.responses.len() - 1);
        // Advance, but never past the last index (so the last response repeats).
        if *cursor < self.responses.len() - 1 {
            *cursor += 1;
        }
        Ok(self.responses[idx].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_returns_in_order_then_repeats_last() {
        let mock = MockLlm::new(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(mock.complete("s", "u").await.unwrap(), "a");
        assert_eq!(mock.complete("s", "u").await.unwrap(), "b");
        // Exhausted: keeps returning the last response.
        assert_eq!(mock.complete("s", "u").await.unwrap(), "b");
        assert_eq!(mock.complete("s", "u").await.unwrap(), "b");
    }

    #[tokio::test]
    async fn single_response_mock_always_returns_it() {
        let mock = MockLlm::new(vec!["only".to_string()]);
        assert_eq!(mock.complete("s", "u").await.unwrap(), "only");
        assert_eq!(mock.complete("s", "u").await.unwrap(), "only");
    }

    #[test]
    fn from_env_errors_when_key_unset() {
        // Snapshot and clear the key for this test's scope, then restore.
        let saved = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let result = AnthropicClient::from_env();
        assert!(
            matches!(result, Err(LlmError::Config(_))),
            "expected Config error when ANTHROPIC_API_KEY is unset, got {result:?}"
        );

        if let Some(v) = saved {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
    }

    #[test]
    fn new_builds_a_client_with_defaults() {
        let client = AnthropicClient::new("sk-test", "claude-test");
        assert_eq!(client.model(), "claude-test");
        assert_eq!(client.base_url(), DEFAULT_BASE_URL);
    }
}
